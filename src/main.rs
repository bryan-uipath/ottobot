//! ottobot: a scheduler that fires coding-agent runs from trigger-bearing skill files.

mod caffeinate;
mod checker;
mod client;
mod config;
mod daemon;
mod otto;
mod proto;
mod run;
mod runner;
mod scheduler;
mod skill;
mod state;
mod store;
mod trigger;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "otto", about = "Higher-order coding agent: triggers → agent runs")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Skills folder (searched recursively). Default: config `skills_root`, else ~/.ottobot/skills.
    root: Option<PathBuf>,
    /// How often to evaluate triggers. Default: config `tick`, else 1m.
    #[arg(long, global = true, value_parser = humantime::parse_duration)]
    tick: Option<Duration>,
    /// Runner used when a skill doesn't specify one. Default: config `runner`, else claude.
    #[arg(long, global = true)]
    runner: Option<String>,
    /// Print discovered skills and exit.
    #[arg(long)]
    list: bool,
    /// Print a stored run's events and exit.
    #[arg(long, value_name = "RUN_ID")]
    show: Option<String>,
    /// Run one skill headless (prints events), then exit. Bypasses dedupe/cooldown.
    #[arg(long, value_name = "SKILL")]
    run: Option<String>,
    /// Print each runner's version and quota usage, then exit.
    #[arg(long)]
    usage: bool,
    /// Evaluate one event trigger now (compiles the checker if needed), print the verdict, exit.
    #[arg(long, value_name = "SKILL")]
    check: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run scheduler + socket server in the foreground (launchd keeps it alive once installed).
    Daemon {
        /// Skills folder. Same default as the TUI.
        root: Option<PathBuf>,
        /// Write ~/Library/LaunchAgents/com.ottobot.daemon.plist and load it.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Unload and remove the LaunchAgent.
        #[arg(long)]
        uninstall: bool,
    },
    /// One-line summary from the running daemon. Exit 1 if none.
    Status {
        #[arg(long)]
        json: bool,
    },
}

/// Scheduler running in this process: shared state, its command channel, caffeinate child.
pub struct Core {
    pub shared: state::Shared,
    pub cmds: tokio::sync::mpsc::Sender<scheduler::Command>,
    pub caff: Option<std::process::Child>,
    /// Released on drop; keep it alive as long as the scheduler runs.
    pub _lock: store::SchedulerLock,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Some(Cmd::Status { json }) => return status(*json).await,
        Some(Cmd::Daemon { uninstall: true, .. }) => return daemon::uninstall(),
        _ => {}
    }
    if cli.usage {
        for r in runner::all() {
            let ver = r.preflight().map_err(|e| format!("{e:#}"));
            let usage = r.usage().map(|u| u.summary()).unwrap_or_else(|e| format!("usage unavailable: {e:#}"));
            match ver {
                Ok(v) => println!("{:<8} ✓ {v:<24} {usage}", r.name()),
                Err(e) => println!("{:<8} ✗ {e}", r.name()),
            }
        }
        return Ok(());
    }

    let cfg = config::load()?;
    let root_arg = match &cli.cmd {
        Some(Cmd::Daemon { root, .. }) => root.clone().or(cli.root.clone()),
        _ => cli.root.clone(),
    };
    let root = config::skills_root(root_arg, &cfg)?;
    let tick = cli.tick.or(cfg.tick).unwrap_or(Duration::from_secs(60));
    let default_runner = cli.runner.clone().or(cfg.runner.clone()).unwrap_or_else(|| "claude".into());
    let store = store::Store::open(&root)?;
    if let Some(Cmd::Daemon { install: true, .. }) = &cli.cmd {
        return daemon::install(&root, &store.dir);
    }
    init_logging(&store.dir)?;
    if let Some(Cmd::Daemon { .. }) = &cli.cmd {
        return daemon::serve(root, store, tick, default_runner).await;
    }

    if cli.list {
        let (skills, errors) = skill::discover(&root);
        for s in &skills {
            let t = trigger::Trigger::from_spec(&s.front.trigger).map(|t| t.summary()).unwrap_or_else(|e| format!("invalid: {e:#}"));
            println!("{:<30} {:<10} {}", s.name, s.hash, t);
        }
        for (p, e) in &errors {
            eprintln!("error {}: {e}", p.display());
        }
        return Ok(());
    }

    if let Some(id) = &cli.show {
        let Some(run) = store.load_runs(usize::MAX).into_iter().find(|r| r.meta.id == *id) else { anyhow::bail!("no run {id}") };
        println!("{}", serde_json::to_string_pretty(&run.meta)?);
        for e in &run.events {
            println!("{e:?}");
        }
        return Ok(());
    }

    if let Some(name) = &cli.check {
        return check_once(&root, &store, &default_runner, name).await;
    }

    // TUI prefers a running daemon; --run stays in-process.
    if cli.run.is_none() {
        if let Some(backend) = connect_daemon(&root).await? {
            return tui::run(backend).await;
        }
    }
    let Core { shared, cmds: tx, mut caff, _lock } = start_core(root, store, tick, default_runner)?;
    let res = match cli.run {
        Some(name) => headless(shared, tx, name).await,
        None => {
            shared.lock().unwrap().log("no daemon; running in-process (otto daemon --install to run in background)");
            tui::run(client::Backend::Local { shared, cmds: tx }).await
        }
    };
    if let Some(c) = caff.as_mut() {
        let _ = c.kill();
    }
    res
}

/// Load runs, preflight runners, caffeinate, and spawn the usage refresher and scheduler.
pub fn start_core(root: PathBuf, store: store::Store, tick: Duration, default_runner: String) -> Result<Core> {
    let lock = store.lock_scheduler()?;
    let mut app = state::AppState { root, store_dir: store.dir.clone(), ..Default::default() };
    app.runs = store.load_runs(200);
    app.runner_health = runner::all().into_iter().map(|r| (r.name().to_string(), r.preflight().map_err(|e| format!("{e:#}")))).collect();
    let caff = caffeinate::start();
    app.caffeinated = caff.is_some();
    let shared: state::Shared = Arc::new(Mutex::new(app));

    tokio::spawn(refresh_usage(shared.clone()));

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let sched = scheduler::Scheduler::new(shared.clone(), store, tick, default_runner);
    tokio::spawn(sched.run(rx));
    Ok(Core { shared, cmds: tx, caff, _lock: lock })
}

/// Remote backend if a daemon answers for `root`. A daemon serving another root is an error, not a fallback.
async fn connect_daemon(root: &std::path::Path) -> Result<Option<client::Backend>> {
    let path = client::socket_path()?;
    let Ok(mut conn) = client::Conn::connect(&path).await else { return Ok(None) };
    let Ok(s) = conn.status().await else { return Ok(None) };
    if s.root != root {
        eprintln!("otto daemon (pid {}) serves a different root:\n  daemon: {}\n  wanted: {}", s.pid, s.root.display(), root.display());
        std::process::exit(1);
    }
    Ok(Some(client::Backend::remote(path, s.root, conn)))
}

/// `otto status`: one line (or JSON) from the daemon.
async fn status(json: bool) -> Result<()> {
    let s = match client::Conn::connect(&client::socket_path()?).await {
        Ok(mut c) => c.status().await,
        Err(e) => Err(e),
    };
    let s = match s {
        Ok(s) => s,
        Err(_) => {
            eprintln!("no daemon");
            std::process::exit(1);
        }
    };
    if json {
        println!("{}", serde_json::to_string(&s)?);
        return Ok(());
    }
    let mut parts = vec![
        format!("daemon {}", s.pid),
        s.root.display().to_string(),
        format!("running {}", s.running),
        format!("skills {}", s.skills),
        format!("next fire {}", s.next_fire.map(|t| t.format("%a %H:%M").to_string()).unwrap_or("—".into())),
    ];
    for (name, u) in &s.runner_usage {
        parts.push(match u {
            Ok(u) => format!("{name} {u}"),
            Err(_) => format!("{name} n/a"),
        });
    }
    println!("{}", parts.join(" · "));
    Ok(())
}

/// Poll runner quota off the UI thread: usage() does network/file IO and may take seconds.
async fn refresh_usage(shared: state::Shared) {
    loop {
        let usage = tokio::task::spawn_blocking(|| {
            runner::all().into_iter().map(|r| (r.name().to_string(), r.usage().map(|u| u.summary()).map_err(|e| format!("{e:#}")))).collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        shared.lock().unwrap().runner_usage = usage;
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    }
}

/// Fire one skill and stream its events to stdout until it finishes.
async fn headless(shared: state::Shared, tx: tokio::sync::mpsc::Sender<scheduler::Command>, name: String) -> Result<()> {
    tx.send(scheduler::Command::RunNow(name.clone())).await?;
    let mut printed = 0;
    let mut logged = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let st = shared.lock().unwrap();
        for (_, m) in st.log.iter().skip(logged) {
            eprintln!("[ottobot] {m}");
        }
        logged = st.log.len();
        let Some(run) = st.runs.iter().find(|r| r.meta.skill == name) else { continue };
        for e in run.events.iter().skip(printed) {
            println!("{e:?}");
        }
        printed = run.events.len();
        if run.meta.status != run::RunStatus::Running {
            println!("status: {:?}", run.meta.status);
            return Ok(());
        }
    }
}

/// Run a skill's event checker once and print the verdict.
async fn check_once(root: &std::path::Path, store: &store::Store, runner: &str, name: &str) -> Result<()> {
    let (skills, _) = skill::discover(root);
    let Some(skill) = skills.into_iter().find(|s| s.name == name) else { anyhow::bail!("no skill {name}") };
    let trigger::Trigger::Event { description, .. } = trigger::Trigger::from_spec(&skill.front.trigger)? else {
        anyhow::bail!("{name} has a cron trigger, nothing to check")
    };
    let c = checker::Checker { skill, description, dir: store.dir.join("checkers"), cwd: root.to_path_buf(), runner: runner.to_string() };
    let out = c.check(0).await?;
    if let Some(e) = &out.script_failed {
        eprintln!("[ottobot] script failed: {e}");
    }
    eprintln!("[ottobot] mode: {:?}, script: {}", out.mode, c.script_path().display());
    println!("{}", serde_json::to_string(&out.result)?);
    Ok(())
}

fn init_logging(dir: &std::path::Path) -> Result<()> {
    let file = tracing_appender::rolling::daily(dir, "ottobot.log");
    tracing_subscriber::fmt().with_writer(file).with_ansi(false).init();
    Ok(())
}
