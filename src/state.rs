//! In-memory app state shared between scheduler and TUI.

use crate::run::{Run, RunEvent, RunStatus};
use crate::skill::Skill;
use crate::trigger::Trigger;
use chrono::{DateTime, Local};
use std::sync::{Arc, Mutex};

pub type Shared = Arc<Mutex<AppState>>;

#[derive(Debug, Clone)]
pub struct SkillView {
    pub skill: Skill,
    pub trigger: Result<Trigger, String>,
    pub next_fire: Option<DateTime<Local>>,
    pub last_fired: Option<DateTime<Local>>,
    pub active_run: Option<String>,
    /// Last scheduler decision for this skill ("in cooldown 5h12m", "already running", ...).
    pub note: String,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub root: std::path::PathBuf,
    pub store_dir: std::path::PathBuf,
    pub skills: Vec<SkillView>,
    pub skill_errors: Vec<(std::path::PathBuf, String)>,
    pub runs: Vec<Run>,
    pub log: Vec<(DateTime<Local>, String)>,
    pub last_tick: Option<DateTime<Local>>,
    pub next_tick: Option<DateTime<Local>>,
    pub caffeinated: bool,
    pub runner_health: Vec<(String, Result<String, String>)>,
    /// Per-runner quota windows, refreshed in the background.
    pub runner_usage: crate::runner::RunnerUsage,
    /// This process's pid when running as `otto daemon`.
    pub daemon_pid: Option<u32>,
    /// AC vs battery; None when unknown.
    pub power: Option<crate::power::Power>,
}

impl AppState {
    pub fn log(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        self.log.push((Local::now(), msg));
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    pub fn run_mut(&mut self, id: &str) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.meta.id == id)
    }

    pub fn push_event(&mut self, id: &str, e: RunEvent) {
        if let Some(r) = self.run_mut(id) {
            r.events.push(e);
        }
    }

    pub fn finish_run(&mut self, id: &str, status: RunStatus) {
        let skill = self.run_mut(id).map(|r| {
            r.meta.finished_at = Some(Local::now());
            r.meta.status = status;
            r.meta.skill.clone()
        });
        if let Some(s) = skill.and_then(|s| self.skills.iter_mut().find(|v| v.skill.name == s)) {
            s.active_run = None;
        }
    }
}
