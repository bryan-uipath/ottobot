//! In-memory app state shared between scheduler and TUI.

use crate::run::{Level, Run, RunEvent, RunMeta, RunStatus};
use crate::skill::Skill;
use crate::trigger::Trigger;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
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

/// Latest critical/immediate verdict per (skill, incident_key). Kept in `alerts.json`, outside the 200-run window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alert {
    pub skill: String,
    pub key: String,
    pub level: Level,
    pub headline: String,
    pub summary: String,
    pub report: Option<std::path::PathBuf>,
    pub run_id: String,
    pub first_seen: DateTime<Local>,
    pub last_seen: DateTime<Local>,
    /// Level the user dismissed at; a strictly higher verdict re-opens the alert.
    pub dismissed_level: Option<Level>,
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
    pub alerts: Vec<Alert>,
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

    /// Upsert the alert for a finished run's verdict; `normal` on the same key clears it.
    /// Failed and unassessed runs change nothing. Returns whether alerts changed.
    pub fn record_verdict(&mut self, m: &RunMeta) -> bool {
        let (RunStatus::Succeeded, Some(v)) = (&m.status, &m.verdict) else { return false };
        let i = self.alerts.iter().position(|a| a.skill == m.skill && a.key == v.incident_key);
        let now = m.finished_at.unwrap_or_else(Local::now);
        match (i, v.level) {
            (None, Level::Normal) => return false,
            (Some(i), Level::Normal) => drop(self.alerts.remove(i)),
            (Some(i), level) => {
                let a = &mut self.alerts[i];
                if a.dismissed_level.is_some_and(|d| level > d) {
                    a.dismissed_level = None;
                }
                (a.level, a.headline, a.summary, a.report, a.run_id, a.last_seen) =
                    (level, v.headline.clone(), v.summary.clone(), m.report.clone(), m.id.clone(), now);
            }
            (None, level) => self.alerts.push(Alert {
                skill: m.skill.clone(),
                key: v.incident_key.clone(),
                level,
                headline: v.headline.clone(),
                summary: v.summary.clone(),
                report: m.report.clone(),
                run_id: m.id.clone(),
                first_seen: now,
                last_seen: now,
                dismissed_level: None,
            }),
        }
        true
    }

    /// Hide an alert until a higher level arrives. False if there is no such alert.
    pub fn dismiss(&mut self, skill: &str, key: &str) -> bool {
        let Some(a) = self.alerts.iter_mut().find(|a| a.skill == skill && a.key == key) else { return false };
        a.dismissed_level = Some(a.level);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Verdict;

    fn run(id: &str, status: RunStatus, verdict: Option<&str>) -> RunMeta {
        RunMeta {
            id: id.into(),
            skill: "s".into(),
            runner: "claude".into(),
            model: None,
            effort: None,
            trigger_key: "t".into(),
            cwd: Default::default(),
            started_at: Local::now(),
            finished_at: Some(Local::now()),
            status,
            summary: None,
            report: None,
            verdict: verdict.and_then(Verdict::parse),
            cost_usd: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn ok(id: &str, level: &str, key: &str) -> RunMeta {
        let v = format!(r#"{{"level":"{level}","incident_key":"{key}","headline":"{id}","evidence":["e"]}}"#);
        run(id, RunStatus::Succeeded, Some(&v))
    }

    fn levels(st: &AppState) -> Vec<(&str, Level, Option<Level>)> {
        st.alerts.iter().map(|a| (a.key.as_str(), a.level, a.dismissed_level)).collect()
    }

    #[test]
    fn upsert_lower_clear() {
        let mut st = AppState::default();
        assert!(!st.record_verdict(&ok("r0", "normal", "a")), "normal with no alert is a no-op");
        assert!(st.record_verdict(&ok("r1", "immediate", "a")));
        assert!(st.record_verdict(&ok("r2", "critical", "b")));
        assert!(st.record_verdict(&ok("r3", "critical", "a")));
        assert_eq!(levels(&st), vec![("a", Level::Critical, None), ("b", Level::Critical, None)]);
        assert_eq!((st.alerts[0].headline.as_str(), st.alerts[0].run_id.as_str()), ("r3", "r3"));
        // Normal on another key leaves "a"; on "a" it clears.
        st.record_verdict(&ok("r4", "normal", "c"));
        st.record_verdict(&ok("r5", "normal", "a"));
        assert_eq!(levels(&st), vec![("b", Level::Critical, None)]);
    }

    #[test]
    fn dismiss_and_reopen_on_escalation() {
        let mut st = AppState::default();
        st.record_verdict(&ok("r1", "critical", "a"));
        assert!(!st.dismiss("s", "nope"));
        assert!(st.dismiss("s", "a"));
        st.record_verdict(&ok("r2", "critical", "a"));
        assert_eq!(levels(&st), vec![("a", Level::Critical, Some(Level::Critical))], "same level stays dismissed");
        st.record_verdict(&ok("r3", "immediate", "a"));
        assert_eq!(levels(&st), vec![("a", Level::Immediate, None)], "higher level re-opens");
    }

    #[test]
    fn unassessed_and_failed_ignored() {
        let mut st = AppState::default();
        st.record_verdict(&ok("r1", "critical", "a"));
        let before = st.alerts.clone();
        assert!(!st.record_verdict(&run("r2", RunStatus::Succeeded, None)));
        assert!(!st.record_verdict(&run("r3", RunStatus::Succeeded, Some("bad json"))));
        let failed = r#"{"level":"normal","incident_key":"a"}"#;
        assert!(!st.record_verdict(&run("r4", RunStatus::Failed("x".into()), Some(failed))));
        assert_eq!(st.alerts, before);
    }
}
