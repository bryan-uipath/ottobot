//! ottobot: a scheduler that fires coding-agent runs from trigger-bearing skill files.

mod caffeinate;
mod config;
mod otto;
mod run;
mod runner;
mod scheduler;
mod skill;
mod state;
mod store;
mod trigger;
mod tui;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "otto", about = "Higher-order coding agent: triggers → agent runs")]
struct Cli {
    /// Skills folder (searched recursively). Default: config `skills_root`, else ~/.ottobot/skills.
    root: Option<PathBuf>,
    /// How often to evaluate triggers. Default: config `tick`, else 1m.
    #[arg(long, value_parser = humantime::parse_duration)]
    tick: Option<Duration>,
    /// Runner used when a skill doesn't specify one. Default: config `runner`, else claude.
    #[arg(long)]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load()?;
    let root = config::skills_root(cli.root.clone(), &cfg)?;
    let tick = cli.tick.or(cfg.tick).unwrap_or(Duration::from_secs(60));
    let default_runner = cli.runner.clone().or(cfg.runner.clone()).unwrap_or_else(|| "claude".into());
    let store = store::Store::open(&root)?;
    init_logging(&store.dir)?;

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

    let mut app = state::AppState { root: root.clone(), store_dir: store.dir.clone(), ..Default::default() };
    app.runs = store.load_runs(200);
    app.runner_health = runner::all().into_iter().map(|r| (r.name().to_string(), r.preflight().map_err(|e| format!("{e:#}")))).collect();
    let mut caff = caffeinate::start();
    app.caffeinated = caff.is_some();
    let shared: state::Shared = Arc::new(Mutex::new(app));

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let sched = scheduler::Scheduler::new(shared.clone(), store, tick, default_runner);
    tokio::spawn(sched.run(rx));

    let res = match cli.run {
        Some(name) => headless(shared, tx, name).await,
        None => tui::run(shared, tx).await,
    };
    if let Some(c) = caff.as_mut() {
        let _ = c.kill();
    }
    res
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

fn init_logging(dir: &std::path::Path) -> Result<()> {
    let file = tracing_appender::rolling::daily(dir, "ottobot.log");
    tracing_subscriber::fmt().with_writer(file).with_ansi(false).init();
    Ok(())
}
