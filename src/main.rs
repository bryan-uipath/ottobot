//! ottobot: a scheduler that fires coding-agent runs from trigger-bearing skill files.

mod caffeinate;
mod checker;
mod client;
mod config;
mod daemon;
mod ledger;
mod otto;
mod power;
mod proto;
mod route;
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
#[command(name = "otto", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("OTTO_COMMIT"), ")"), about = "Higher-order coding agent: triggers → agent runs")]
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
    /// A skill's `$OTTO_LEDGER` items (ids it already handled). File ops only; no daemon needed.
    Ledger {
        skill: String,
        /// Forget one item so the trigger sees it again.
        #[arg(long, value_name = "ID", conflicts_with = "clear")]
        rm: Option<String>,
        /// Forget every item (other top-level keys are kept).
        #[arg(long)]
        clear: bool,
    },
    /// Reports from past runs, newest first (latest per skill unless --all). Reads the store; no daemon needed.
    Reports {
        /// Only this skill.
        skill: Option<String>,
        /// Every report, not just the latest per skill.
        #[arg(long)]
        all: bool,
        /// Open the newest matching report instead of listing; without a skill, the reports index.
        #[arg(long)]
        open: bool,
    },
}

/// Scheduler running in this process: shared state, its command channel, caffeinate child.
pub struct Core {
    pub shared: state::Shared,
    pub cmds: tokio::sync::mpsc::Sender<scheduler::Command>,
    caff: Option<std::process::Child>,
    /// Released on drop; keep it alive as long as the scheduler runs.
    _lock: store::FileLock,
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Some(c) = self.caff.as_mut() {
            let _ = c.kill();
        }
    }
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
            let usage = r.usage().map(|u| runner::summary(&u.windows)).unwrap_or_else(|e| format!("usage unavailable: {e:#}"));
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
        // Forward explicit overrides; the agent otherwise runs with config/defaults.
        let mut extra = vec![];
        if let Some(t) = cli.tick {
            extra.extend(["--tick".to_string(), humantime::format_duration(t).to_string()]);
        }
        if let Some(r) = &cli.runner {
            extra.extend(["--runner".to_string(), r.clone()]);
        }
        return daemon::install(&root, &store.dir, &extra);
    }
    init_logging(&store.dir)?;
    if let Some(Cmd::Daemon { .. }) = &cli.cmd {
        return daemon::serve(root, store, tick, default_runner, cfg.models.clone()).await;
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

    if let Some(Cmd::Reports { skill, all, open }) = &cli.cmd {
        return reports(&store, skill.as_deref(), *all, *open);
    }

    if let Some(Cmd::Ledger { skill, rm, clear }) = &cli.cmd {
        return ledger(&root, &store, skill, rm.as_deref(), *clear);
    }

    if let Some(name) = &cli.check {
        return check_once(&root, &store, &default_runner, name).await;
    }

    // A running daemon owns the scheduler: TUI and --run both go through it.
    if let Some(backend) = connect_daemon(&root).await? {
        if cli.tick.is_some() || cli.runner.is_some() {
            eprintln!("note: --tick/--runner are ignored while attached to a daemon");
        }
        return match cli.run {
            Some(name) => headless(backend, name).await,
            None => tui::run(backend).await,
        };
    }
    let lock = store.lock_scheduler()?;
    let core = start_core(root, store, tick, default_runner, cfg.models.clone(), lock);
    let backend = client::Backend::Local { shared: core.shared.clone(), cmds: core.cmds.clone() };
    match cli.run {
        Some(name) => headless(backend, name).await,
        None => {
            core.shared.lock().unwrap().log("no daemon; running in-process (otto daemon --install to run in background)");
            tui::run(backend).await
        }
    }
}

/// Load runs, preflight runners, caffeinate, and spawn the usage refresher and scheduler.
pub fn start_core(
    root: PathBuf,
    store: store::Store,
    tick: Duration,
    default_runner: String,
    aliases: std::collections::HashMap<String, Vec<String>>,
    lock: store::FileLock,
) -> Core {
    let mut app = state::AppState { root, store_dir: store.dir.clone(), ..Default::default() };
    app.runs = store.load_runs(200);
    if let Err(e) = store.write_index() {
        tracing::warn!("reports index: {e:#}");
    }
    crate::store::AlertsFile { alerts: app.alerts, notified: app.notified, dismissed_failures: app.dismissed_failures } =
        store.load_alerts();
    app.runner_health = runner::all().into_iter().map(|r| (r.name().to_string(), r.preflight().map_err(|e| format!("{e:#}")))).collect();
    let caff = caffeinate::start();
    app.caffeinated = caff.is_some();
    let shared: state::Shared = Arc::new(Mutex::new(app));

    tokio::spawn(refresh_usage(shared.clone()));
    tokio::spawn(refresh_power(shared.clone()));

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let sched = scheduler::Scheduler::new(shared.clone(), store, tick, default_runner, aliases);
    tokio::spawn(sched.run(rx));
    Core { shared, cmds: tx, caff, _lock: lock }
}

/// Remote backend if a daemon answers for `root`. A daemon serving another root is an error, not a fallback.
async fn connect_daemon(root: &std::path::Path) -> Result<Option<client::Backend>> {
    let path = client::socket_path()?;
    let Some((conn, s)) = find_or_exit(&path).await else { return Ok(None) };
    if s.root != root {
        eprintln!("otto daemon (pid {}) serves a different root:\n  daemon: {}\n  wanted: {}", s.pid, s.root.display(), root.display());
        std::process::exit(1);
    }
    Ok(Some(client::Backend::remote(path, s.root, conn)))
}

/// `Conn::find`, exiting 1 on a daemon we can't talk to (e.g. older protocol).
async fn find_or_exit(path: &std::path::Path) -> Option<(client::Conn, proto::Status)> {
    client::Conn::find(path).await.unwrap_or_else(|e| {
        eprintln!("{e:#}");
        std::process::exit(1);
    })
}

/// `otto status`: one line (or JSON) from the daemon.
async fn status(json: bool) -> Result<()> {
    let Some((_, s)) = find_or_exit(&client::socket_path()?).await else {
        eprintln!("no daemon");
        std::process::exit(1);
    };
    if json {
        println!("{}", serde_json::to_string(&s)?);
        return Ok(());
    }
    let mut parts = vec![
        format!("daemon {} ({})", s.pid, if s.commit.is_empty() { "?" } else { &s.commit }),
        s.root.display().to_string(),
        format!("running {}", s.running),
        format!("skills {}", s.skills),
        format!("next fire {}", s.next_fire.map(|t| t.format("%a %H:%M").to_string()).unwrap_or("—".into())),
    ];
    if let Some(p) = &s.power {
        parts.push(p.summary());
    }
    for (name, u) in &s.runner_usage {
        parts.push(match u {
            Ok(w) => format!("{name} {}", runner::summary(w)),
            Err(_) => format!("{name} n/a"),
        });
    }
    println!("{}", parts.join(" · "));
    Ok(())
}

/// Poll runner quota off the UI thread: usage() does network/file IO and may take seconds.
async fn refresh_usage(shared: state::Shared) {
    loop {
        let usage = tokio::task::spawn_blocking(|| -> runner::RunnerUsage {
            runner::all().into_iter().map(|r| (r.name().to_string(), r.usage().map(|u| u.windows).map_err(|e| format!("{e:#}")))).collect()
        })
        .await
        .unwrap_or_default();
        shared.lock().unwrap().runner_usage = usage;
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    }
}

/// `otto reports`: list or open report files recorded in run metadata.
fn reports(store: &store::Store, skill: Option<&str>, all: bool, open: bool) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let runs: Vec<_> = store
        .load_metas()
        .into_iter()
        .filter(|m| skill.is_none_or(|s| m.skill == s))
        .filter(|m| m.report.as_ref().is_some_and(|p| p.is_file()))
        .filter(|m| all || seen.insert(m.skill.clone()))
        .collect();
    // The index exists (and is useful) even before the first report.
    if open && skill.is_none() {
        std::process::Command::new("open").arg(store.write_index()?).status()?;
        return Ok(());
    }
    if runs.is_empty() {
        anyhow::bail!("no reports{}", skill.map(|s| format!(" for {s}")).unwrap_or_default());
    }
    if open {
        let Some(p) = runs[0].report.as_ref() else { return Ok(()) };
        std::process::Command::new("open").arg(p).status()?;
        return Ok(());
    }
    for m in &runs {
        let status = match &m.status {
            run::RunStatus::Succeeded => "✓",
            run::RunStatus::Failed(_) => "✗",
            run::RunStatus::Running => "…",
        };
        println!(
            "{status} {:<16} {}  {}",
            m.skill,
            m.started_at.format("%Y-%m-%d %H:%M"),
            m.report.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        );
    }
    println!("\n{}", store.dir.join("reports").display());
    Ok(())
}

/// `otto ledger`: list, forget one, or clear a skill's handled items.
fn ledger(root: &std::path::Path, store: &store::Store, name: &str, rm: Option<&str>, clear: bool) -> Result<()> {
    if !skill::discover(root).0.iter().any(|s| s.name == name) {
        anyhow::bail!("no skill {name}");
    }
    let path = store.ledger_path(name);
    let (mut v, was) = ledger::load(&path)?;
    let Some(items) = ledger::items(&mut v) else { anyhow::bail!("{}: no items", path.display()) };
    if let Some(id) = rm {
        if items.remove(id).is_none() {
            anyhow::bail!("{id} not in {}", path.display());
        }
        println!("removed {id}");
    } else if clear {
        println!("cleared {} item(s)", items.len());
        items.clear();
    } else {
        let field = |v: &serde_json::Value| runner::truncate(&v.as_str().map(String::from).unwrap_or_else(|| v.to_string()), 40);
        for (id, item) in items.iter() {
            let at = item.get("at").map(field).unwrap_or_else(|| "—".into());
            let rest: Vec<String> = item
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| *k != "at")
                .take(3)
                .map(|(k, v)| format!("{k}={}", field(v)))
                .collect();
            println!("{id:<24} {at:<26} {}", rest.join(" "));
        }
        println!("\n{} item(s) · {}", items.len(), path.display());
        return Ok(());
    }
    // Nothing on disk means nothing to rewrite (and the memory dir may not exist).
    if path.exists() {
        ledger::save(&path, &v, &was)?;
    }
    Ok(())
}

/// Poll the power source every 30s so a switch to battery shows up quickly.
async fn refresh_power(shared: state::Shared) {
    loop {
        let p = tokio::task::spawn_blocking(power::read).await.unwrap_or_default();
        shared.lock().unwrap().power = p;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// Fire one skill and stream its events to stdout until it finishes. Works against either backend.
async fn headless(mut backend: client::Backend, name: String) -> Result<()> {
    // In-process, the scheduler's first reload races us; wait for skills to appear.
    let mut logged = 0;
    for _ in 0..25 {
        let snap = backend.snapshot().await?;
        logged = snap.log.len();
        if !snap.skills.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Tolerate clock skew between client and daemon when matching the run we asked for.
    let asked = chrono::Local::now() - chrono::Duration::seconds(2);
    backend.run_now(&name).await?;
    let mut printed = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snap = backend.snapshot().await?;
        for (_, m) in snap.log.iter().skip(logged) {
            eprintln!("[ottobot] {m}");
        }
        logged = snap.log.len();
        let Some(run) = snap.runs.iter().filter(|r| r.skill == name && r.started_at >= asked).max_by_key(|r| r.started_at) else {
            continue;
        };
        for e in backend.events(&run.id, printed).await? {
            println!("{e:?}");
            printed += 1;
        }
        if run.status != run::RunStatus::Running {
            println!("status: {:?}", run.status);
            return Ok(());
        }
    }
}

/// Run a skill's event checker once and print the verdict.
async fn check_once(root: &std::path::Path, store: &store::Store, runner: &str, name: &str) -> Result<()> {
    let (skills, _) = skill::discover(root);
    let Some(skill) = skills.into_iter().find(|s| s.name == name) else { anyhow::bail!("no skill {name}") };
    let trigger::Trigger::Event { description, watch, .. } = trigger::Trigger::from_spec(&skill.front.trigger)? else {
        anyhow::bail!("{name} has a cron trigger, nothing to check")
    };
    // A diagnostic check must not rewrite the live notebook the daemon relies on: work on a throwaway copy.
    let live = store.memory_dir(&skill.name);
    // Under the 0700 store, created non-recursively with 0700 so a pre-existing path (symlink) fails instead of being used.
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let memory = store.dir.join("tmp").join(format!("check-{}-{}-{nonce}", skill.name, std::process::id()));
    std::fs::create_dir_all(store.dir.join("tmp"))?;
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&memory)?;
    }
    for e in std::fs::read_dir(&live)?.flatten() {
        let _ = std::fs::copy(e.path(), memory.join(e.file_name()));
    }
    let c = checker::Checker::new(skill, description, watch, store, root, runner).with_memory(memory.clone());
    let out = c.check(0).await;
    let _ = std::fs::remove_dir_all(&memory);
    let out = out?;
    if let Some(e) = &out.script_failed {
        eprintln!("[ottobot] script failed: {e}");
    }
    if watch {
        eprintln!("[ottobot] mode: {:?} (watch; memory copy discarded, live notes untouched)", out.mode);
    } else {
        eprintln!("[ottobot] mode: {:?}, script: {}", out.mode, c.script_path().display());
    }
    println!("{}", serde_json::to_string(&out.result)?);
    Ok(())
}

fn init_logging(dir: &std::path::Path) -> Result<()> {
    let file = tracing_appender::rolling::daily(dir, "ottobot.log");
    tracing_subscriber::fmt().with_writer(file).with_ansi(false).init();
    Ok(())
}
