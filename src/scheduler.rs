//! Tick loop: rediscover skills, evaluate triggers, dedupe, spawn runs.

use crate::run::{Run, RunEvent, RunMeta, RunStatus};
use crate::runner::{self, Invocation};
use crate::skill::{self, Skill};
use crate::state::{Shared, SkillView};
use crate::store::{SkillState, Store};
use crate::trigger::Trigger;
use anyhow::Result;
use chrono::{DateTime, Local};
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
}

impl Scheduler {
    pub fn new(shared: Shared, store: Store, tick: Duration, default_runner: String) -> Self {
        Self { shared, store, tick, default_runner, last_eval: Local::now() }
    }

    pub async fn run(mut self, mut cmds: mpsc::Receiver<Command>) {
        self.reload();
        let mut interval = tokio::time::interval(self.tick);
        loop {
            tokio::select! {
                _ = interval.tick() => self.tick_once(),
                Some(cmd) = cmds.recv() => match cmd {
                    Command::Reload => self.reload(),
                    Command::RunNow(name) => self.fire(&name, "manual"),
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
                let next_fire = trigger.as_ref().ok().and_then(|t| t.next_after(now));
                SkillView {
                    last_fired: entry.last_fired_at,
                    active_run: None,
                    note: match &trigger {
                        Ok(Trigger::Event { .. }) => "event triggers not yet evaluated".into(),
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
        // Preserve active-run markers across reloads.
        let active: Vec<(String, Option<String>)> = sh.skills.iter().map(|v| (v.skill.name.clone(), v.active_run.clone())).collect();
        let mut views = views;
        for v in &mut views {
            if let Some((_, a)) = active.iter().find(|(n, _)| *n == v.skill.name) {
                v.active_run = a.clone();
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
            self.fire(&name, &key);
        }
    }

    /// Dedupe gate then spawn. `key` identifies the trigger occurrence.
    fn fire(&mut self, name: &str, key: &str) {
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
        if let Err(e) = self.spawn(skill, id.clone(), key) {
            self.shared.lock().unwrap().log(format!("{name}: failed to start: {e:#}"));
        }
    }

    fn spawn(&self, skill: Skill, id: String, key: &str) -> Result<()> {
        let runner_name = skill.front.runner.clone().unwrap_or_else(|| self.default_runner.clone());
        let runner = runner::by_name(&runner_name)?;
        let root = self.shared.lock().unwrap().root.clone();
        let inv = Invocation {
            prompt: build_prompt(&skill, key),
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
fn build_prompt(skill: &Skill, key: &str) -> String {
    format!(
        "You are running unattended as an ottobot job for skill `{}` (trigger: {}). \
         Do not ask questions; make reasonable decisions and finish. \
         Never push to a protected branch.\n\n{}",
        skill.name, key, skill.body
    )
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
