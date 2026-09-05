//! Tick loop: rediscover skills, evaluate triggers, dedupe, spawn runs.

use crate::checker::{Checker, Outcome};
use crate::run::{Run, RunEvent, RunMeta, RunStatus};
use crate::runner::{self, Invocation};
use crate::skill::{self, Skill};
use crate::state::{Shared, SkillView};
use crate::store::{SkillState, Store};
use crate::trigger::Trigger;
use anyhow::Result;
use chrono::{DateTime, Local};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

pub enum Command {
    RunNow(String),
    Reload,
}

pub struct Scheduler {
    pub shared: Shared,
    pub store: Store,
    pub tick: Duration,
    pub default_runner: String,
    /// Cron occurrences are evaluated strictly after this; advanced each tick.
    last_eval: DateTime<Local>,
    /// Event checks in flight, and the channel their verdicts come back on.
    checking: HashSet<String>,
    check_tx: mpsc::UnboundedSender<(String, Result<Outcome, String>)>,
    check_rx: mpsc::UnboundedReceiver<(String, Result<Outcome, String>)>,
}

impl Scheduler {
    pub fn new(shared: Shared, store: Store, tick: Duration, default_runner: String) -> Self {
        let (check_tx, check_rx) = mpsc::unbounded_channel();
        Self { shared, store, tick, default_runner, last_eval: Local::now(), checking: HashSet::new(), check_tx, check_rx }
    }

    pub async fn run(mut self, mut cmds: mpsc::Receiver<Command>) {
        self.reload();
        let mut interval = tokio::time::interval(self.tick);
        loop {
            tokio::select! {
                _ = interval.tick() => self.tick_once(),
                Some((name, out)) = self.check_rx.recv() => self.on_check(&name, out),
                Some(cmd) = cmds.recv() => match cmd {
                    Command::Reload => self.reload(),
                    Command::RunNow(name) => self.fire(&name, "manual", None),
                },
            }
        }
    }

    /// Rescan disk. Trigger-hash changes reset that skill's dedupe state.
    fn reload(&mut self) {
        let root = self.shared.lock().unwrap().root.clone();
        let (skills, errors) = skill::discover(&root);
        let mut st = self.store.load_state();
        let now = Local::now();
        let views: Vec<SkillView> = skills
            .into_iter()
            .map(|s| {
                let entry = st.skills.entry(s.name.clone()).or_default();
                if entry.trigger_hash != s.trigger_hash {
                    if !entry.trigger_hash.is_empty() {
                        tracing::info!("{}: trigger changed, resetting state", s.name);
                    }
                    *entry = SkillState { hash: s.hash.clone(), trigger_hash: s.trigger_hash.clone(), ..Default::default() };
                }
                entry.hash = s.hash.clone();
                let trigger = Trigger::from_spec(&s.front.trigger).map_err(|e| format!("{e:#}"));
                // Event: next_fire is the next check, not a known fire time.
                let next_fire = match &trigger {
                    Ok(Trigger::Event { check_every, .. }) => Some(next_check(entry.last_checked_at, *check_every, now)),
                    Ok(t) => t.next_after(now),
                    Err(_) => None,
                };
                SkillView {
                    last_fired: entry.last_fired_at,
                    active_run: None,
                    note: match &trigger {
                        Ok(_) => String::new(),
                        Err(e) => format!("invalid: {e}"),
                    },
                    skill: s,
                    trigger,
                    next_fire,
                }
            })
            .collect();
        let _ = self.store.save_state(&st);
        let mut sh = self.shared.lock().unwrap();
        // Preserve active-run markers and last decision notes across reloads.
        let prev: Vec<(String, Option<String>, String)> = sh.skills.iter().map(|v| (v.skill.name.clone(), v.active_run.clone(), v.note.clone())).collect();
        let mut views = views;
        for v in &mut views {
            if let Some((_, a, n)) = prev.iter().find(|(n, _, _)| *n == v.skill.name) {
                v.active_run = a.clone();
                if v.note.is_empty() {
                    v.note = n.clone();
                }
            }
        }
        sh.log(format!("loaded {} skill(s), {} error(s)", views.len(), errors.len()));
        sh.skills = views;
        sh.skill_errors = errors;
    }

    fn tick_once(&mut self) {
        let now = Local::now();
        self.reload();
        let due: Vec<(String, String)> = {
            let mut sh = self.shared.lock().unwrap();
            sh.last_tick = Some(now);
            sh.next_tick = Some(now + chrono::Duration::from_std(self.tick).unwrap_or_default());
            sh.skills
                .iter()
                .filter_map(|v| match &v.trigger {
                    Ok(t @ Trigger::Cron { .. }) => t
                        .next_after(self.last_eval)
                        .filter(|n| *n <= now)
                        .map(|n| (v.skill.name.clone(), n.format("%Y-%m-%dT%H:%M").to_string())),
                    _ => None,
                })
                .collect()
        };
        self.last_eval = now;
        for (name, key) in due {
            self.fire(&name, &key, None);
        }
        self.start_checks(now);
    }

    /// Spawn a checker for each event skill whose poll interval has elapsed.
    fn start_checks(&mut self, now: DateTime<Local>) {
        let mut st = self.store.load_state();
        let root = self.shared.lock().unwrap().root.clone();
        let mut sh = self.shared.lock().unwrap();
        for v in sh.skills.iter_mut() {
            let Ok(Trigger::Event { description, check_every }) = &v.trigger else { continue };
            let name = v.skill.name.clone();
            if self.checking.contains(&name) || v.active_run.is_some() {
                continue;
            }
            let entry = st.skills.entry(name.clone()).or_default();
            if next_check(entry.last_checked_at, *check_every, now) > now {
                continue;
            }
            entry.last_checked_at = Some(now);
            v.next_fire = Some(now + chrono::Duration::from_std(*check_every).unwrap_or_default());
            v.note = "checking…".into();
            self.checking.insert(name.clone());
            let checker = Checker {
                skill: v.skill.clone(),
                description: description.clone(),
                dir: self.store.dir.join("checkers"),
                cwd: root.clone(),
                runner: v.skill.front.runner.clone().unwrap_or_else(|| self.default_runner.clone()),
            };
            let failures = entry.checker_failures;
            let tx = self.check_tx.clone();
            tokio::spawn(async move {
                let out = checker.check(failures).await.map_err(|e| format!("{e:#}"));
                let _ = tx.send((name, out));
            });
        }
        drop(sh);
        let _ = self.store.save_state(&st);
    }

    /// Checker verdict: record, annotate the skill, fire if the condition holds.
    fn on_check(&mut self, name: &str, out: Result<Outcome, String>) {
        self.checking.remove(name);
        let mut st = self.store.load_state();
        let entry = st.skills.entry(name.to_string()).or_default();
        let (note, fire) = match &out {
            Ok(o) => {
                match &o.script_failed {
                    Some(_) => entry.checker_failures += 1,
                    None => entry.checker_failures = 0,
                }
                let mode = format!("{:?}", o.mode).to_lowercase();
                if o.result.fired {
                    (format!("fired ({mode}) · {}", o.result.summary), Some((o.result.key.clone(), o.result.summary.clone())))
                } else {
                    (format!("not fired ({mode}) · {}", o.result.summary), None)
                }
            }
            Err(e) => (format!("check failed: {e}"), None),
        };
        let _ = self.store.save_state(&st);
        {
            let mut sh = self.shared.lock().unwrap();
            sh.log(format!("{name}: {note}"));
            if let Some(v) = sh.skills.iter_mut().find(|v| v.skill.name == name) {
                v.note = note;
            }
        }
        if let Some((key, summary)) = fire {
            let key = if key.is_empty() { Local::now().format("%Y-%m-%dT%H:%M").to_string() } else { key };
            self.fire(name, &key, Some(&summary));
        }
    }

    /// Dedupe gate then spawn. `key` identifies the trigger occurrence; `context` is the checker summary.
    fn fire(&mut self, name: &str, key: &str, context: Option<&str>) {
        let mut st = self.store.load_state();
        let (skill, trigger, active) = {
            let sh = self.shared.lock().unwrap();
            let Some(v) = sh.skills.iter().find(|v| v.skill.name == name) else { return };
            (v.skill.clone(), v.trigger.clone(), v.active_run.clone())
        };
        let Ok(trigger) = trigger else { return };
        let entry = st.skills.entry(name.to_string()).or_default();
        let manual = key == "manual";
        let skip = if active.is_some() {
            Some("already running".to_string())
        } else if !manual && entry.last_fired_key.as_deref() == Some(key) {
            Some(format!("already fired for {key}"))
        } else if !manual {
            let cooldown = skill.front.trigger.cooldown.unwrap_or_else(|| trigger.default_cooldown());
            entry.last_fired_at.and_then(|t| {
                let left = cooldown.checked_sub((Local::now() - t).to_std().unwrap_or_default())?;
                (left > Duration::ZERO).then(|| format!("in cooldown, {} left", humantime::format_duration(Duration::from_secs(left.as_secs()))))
            })
        } else {
            None
        };
        if let Some(reason) = skip {
            let mut sh = self.shared.lock().unwrap();
            sh.log(format!("{name}: skipped ({reason})"));
            if let Some(v) = sh.skills.iter_mut().find(|v| v.skill.name == name) {
                v.note = reason;
            }
            return;
        }
        let id = Run::new_id(name);
        entry.last_fired_at = Some(Local::now());
        entry.last_fired_key = Some(key.to_string());
        entry.last_run_id = Some(id.clone());
        let _ = self.store.save_state(&st);
        if let Err(e) = self.spawn(skill, id.clone(), key, context) {
            self.shared.lock().unwrap().log(format!("{name}: failed to start: {e:#}"));
        }
    }

    fn spawn(&self, skill: Skill, id: String, key: &str, context: Option<&str>) -> Result<()> {
        let runner_name = skill.front.runner.clone().unwrap_or_else(|| self.default_runner.clone());
        let runner = runner::by_name(&runner_name)?;
        let root = self.shared.lock().unwrap().root.clone();
        let inv = Invocation {
            prompt: build_prompt(&skill, key, context),
            cwd: root,
            model: skill.front.model.clone(),
            permission_mode: skill.front.permission_mode.clone(),
        };
        let meta = RunMeta {
            id: id.clone(),
            skill: skill.name.clone(),
            runner: runner_name,
            model: skill.front.model.clone(),
            trigger_key: key.to_string(),
            cwd: inv.cwd.clone(),
            started_at: Local::now(),
            finished_at: None,
            status: RunStatus::Running,
            cost_usd: None,
            input_tokens: 0,
            output_tokens: 0,
        };
        self.store.save_run_meta(&meta)?;
        {
            let mut sh = self.shared.lock().unwrap();
            sh.runs.insert(0, Run { meta: meta.clone(), events: vec![] });
            if let Some(v) = sh.skills.iter_mut().find(|v| v.skill.name == skill.name) {
                v.active_run = Some(id.clone());
                v.last_fired = Some(meta.started_at);
                v.note = "running".into();
            }
            sh.log(format!("{}: started run {id}", skill.name));
        }
        let shared = self.shared.clone();
        let store = self.store.clone();
        let timeout = skill.front.timeout.unwrap_or(Duration::from_secs(60 * 60));
        tokio::spawn(async move {
            let status = match execute(runner, inv, &id, &shared, &store, timeout).await {
                Ok(true) => RunStatus::Succeeded,
                Ok(false) => RunStatus::Failed("runner reported error".into()),
                Err(e) => RunStatus::Failed(format!("{e:#}")),
            };
            let mut sh = shared.lock().unwrap();
            sh.finish_run(&id, status.clone());
            if let Some(r) = sh.run_mut(&id) {
                let _ = store.save_run_meta(&r.meta);
            }
            sh.log(format!("run {id}: {status:?}"));
        });
        Ok(())
    }
}

/// Skill body plus an unattended-run preamble with trigger context.
fn build_prompt(skill: &Skill, key: &str, context: Option<&str>) -> String {
    let ctx = context.map(|c| format!("\nTrigger context: {c}")).unwrap_or_default();
    format!(
        "You are running unattended as an ottobot job for skill `{}` (trigger: {}).{} \
         Do not ask questions; make reasonable decisions and finish. \
         Never push to a protected branch.\n\n{}",
        skill.name, key, ctx, skill.body
    )
}

fn next_check(last: Option<DateTime<Local>>, every: Duration, now: DateTime<Local>) -> DateTime<Local> {
    match last {
        Some(t) => t + chrono::Duration::from_std(every).unwrap_or_default(),
        None => now,
    }
}

/// Stream the child's stdout/stderr into run events. Returns runner's ok flag.
async fn execute(
    runner: Box<dyn runner::Runner>,
    inv: Invocation,
    id: &str,
    shared: &Shared,
    store: &Store,
    timeout: Duration,
) -> Result<bool> {
    let mut cmd = runner.command(&inv);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (tx, mut rx) = mpsc::unbounded_channel::<RunEvent>();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = tx2.send(RunEvent::Stderr(l));
        }
    });
    let runner_ref = std::sync::Arc::new(runner);
    let r2 = runner_ref.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            for e in r2.parse_line(&l) {
                let _ = tx.send(e);
            }
        }
    });
    let mut ok = false;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(e) => {
                    let _ = store.append_event(id, &e);
                    let mut sh = shared.lock().unwrap();
                    match &e {
                        RunEvent::Done { ok: o, .. } => ok = *o,
                        RunEvent::Usage { input, output, cost_usd } => {
                            if let Some(r) = sh.run_mut(id) {
                                r.meta.input_tokens = *input;
                                r.meta.output_tokens = *output;
                                r.meta.cost_usd = *cost_usd;
                            }
                        }
                        _ => {}
                    }
                    sh.push_event(id, e);
                }
                None => break,
            },
            _ = &mut deadline => {
                let _ = child.kill().await;
                anyhow::bail!("timed out after {}", humantime::format_duration(timeout));
            }
        }
    }
    let status = child.wait().await?;
    Ok(ok && status.success())
}
