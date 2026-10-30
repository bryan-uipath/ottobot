//! Daemon wire protocol and the snapshot DTO the TUI draws from.
//! Newline-delimited JSON over a Unix socket: one `Request` line → one `Response` line.

use crate::run::{RunEvent, RunMeta, RunStatus};
use crate::scheduler::Command;
use crate::state::{Alert, AppState, Shared};
use crate::store::Store;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Short SHA this binary was built from (`-dirty` if uncommitted changes); see build.rs.
pub const COMMIT: &str = env!("OTTO_COMMIT");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: PathBuf,
    pub store_dir: PathBuf,
    pub skills: Vec<SkillSnap>,
    pub skill_errors: Vec<(PathBuf, String)>,
    /// Metadata only; events are fetched per run via `Request::Events`.
    pub runs: Vec<RunMeta>,
    pub log: Vec<(DateTime<Local>, String)>,
    pub last_tick: Option<DateTime<Local>>,
    pub next_tick: Option<DateTime<Local>>,
    pub caffeinated: bool,
    pub runner_health: Vec<(String, Result<String, String>)>,
    pub runner_usage: crate::runner::RunnerUsage,
    /// Set when served by a daemon; None in-process.
    pub daemon_pid: Option<u32>,
    pub power: Option<crate::power::Power>,
    /// Undismissed critical/immediate alerts; age = now - `first_seen`.
    #[serde(default)] // a pre-alerts daemon omits it
    pub alerts: Vec<Alert>,
    /// Server's `COMMIT`; empty from a daemon that predates it.
    #[serde(default)]
    pub commit: String,
    /// Skill → dismissed failed run id; see `failures`.
    #[serde(default)]
    pub dismissed_failures: BTreeMap<String, String>,
}

impl Snapshot {
    /// ATTENTION runs: per skill, the latest finished run if it failed in the last 24h and wasn't dismissed.
    pub fn failures(&self) -> Vec<&RunMeta> {
        let since = Local::now() - chrono::Duration::hours(24);
        let mut seen = std::collections::HashSet::new();
        self.runs
            .iter()
            .filter(|r| r.status != RunStatus::Running && seen.insert(&r.skill))
            .filter(|r| matches!(r.status, RunStatus::Failed(_)) && r.started_at >= since)
            .filter(|r| self.dismissed_failures.get(&r.skill) != Some(&r.id))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSnap {
    pub name: String,
    pub description: Option<String>,
    pub runner: Option<String>,
    pub model: Option<String>,
    /// `Trigger::summary()`, or the parse error.
    pub trigger: Result<String, String>,
    pub next_fire: Option<DateTime<Local>>,
    pub last_fired: Option<DateTime<Local>>,
    pub active_run: Option<String>,
    pub note: String,
}

/// Small summary for `otto status` and a tray icon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub root: PathBuf,
    pub pid: u32,
    pub running: usize,
    pub skills: usize,
    /// Newest failed run: (skill, run id).
    pub last_failure: Option<(String, String)>,
    pub next_fire: Option<DateTime<Local>>,
    pub runner_usage: crate::runner::RunnerUsage,
    pub power: Option<crate::power::Power>,
    #[serde(default)]
    pub commit: String,
}

/// e.g. `{"type":"Events","run":"20260101-120000-x","from":0}`, `{"type":"Status"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Snapshot,
    /// Events of one run starting at index `from` (the count the client already has).
    Events {
        run: String,
        from: usize,
    },
    RunNow {
        skill: String,
    },
    Reload,
    Status,
    /// Hide alert (skill, incident key) until a higher level arrives.
    Dismiss {
        skill: String,
        key: String,
    },
    /// Claim alert's pending notification (`notify`); Ok only for the first claim, so it's posted once.
    Notified {
        skill: String,
        key: String,
    },
    /// Hide a failed run from ATTENTION; the skill's next failure shows again.
    DismissFailure {
        run: String,
    },
}

/// e.g. `{"type":"Ok"}`, `{"type":"Status","data":{...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Response {
    Snapshot(Snapshot),
    Events(Vec<RunEvent>),
    Ok,
    Status(Status),
    Error(String),
}

impl AppState {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            root: self.root.clone(),
            store_dir: self.store_dir.clone(),
            skills: self
                .skills
                .iter()
                .map(|v| SkillSnap {
                    name: v.skill.name.clone(),
                    description: v.skill.front.description.clone(),
                    runner: v.skill.front.runner.clone(),
                    model: v.skill.front.model.as_ref().map(|m| m.summary()),
                    trigger: v.trigger.as_ref().map(|t| t.summary()).map_err(Clone::clone),
                    next_fire: v.next_fire,
                    last_fired: v.last_fired,
                    active_run: v.active_run.clone(),
                    note: v.note.clone(),
                })
                .collect(),
            skill_errors: self.skill_errors.clone(),
            // Diagnostics stay in the run record and reports; the tray doesn't need ~6KB/run every poll.
            runs: self.runs.iter().map(|r| RunMeta { stderr_tail: vec![], command: None, ..r.meta.clone() }).collect(),
            log: self.log.clone(),
            last_tick: self.last_tick,
            next_tick: self.next_tick,
            caffeinated: self.caffeinated,
            runner_health: self.runner_health.clone(),
            runner_usage: self.runner_usage.clone(),
            daemon_pid: self.daemon_pid,
            power: self.power.clone(),
            // `normal` is never stored, so undismissed = active.
            alerts: self.alerts.iter().filter(|a| a.dismissed_level.is_none()).cloned().collect(),
            commit: COMMIT.into(),
            dismissed_failures: self.dismissed_failures.clone(),
        }
    }

    pub fn events(&self, id: &str, from: usize) -> Vec<RunEvent> {
        self.runs.iter().find(|r| r.meta.id == id).map(|r| r.events.iter().skip(from).cloned().collect()).unwrap_or_default()
    }

    pub fn status(&self) -> Status {
        Status {
            root: self.root.clone(),
            pid: std::process::id(),
            running: self.runs.iter().filter(|r| r.meta.status == RunStatus::Running).count(),
            skills: self.skills.len(),
            last_failure: self
                .runs
                .iter()
                .find(|r| matches!(r.meta.status, RunStatus::Failed(_)))
                .map(|r| (r.meta.skill.clone(), r.meta.id.clone())),
            next_fire: self.skills.iter().filter_map(|v| v.next_fire).min(),
            runner_usage: self.runner_usage.clone(),
            power: self.power.clone(),
            commit: COMMIT.into(),
        }
    }
}

/// Serve one request against in-process state. Used by the daemon server and the Local backend.
pub async fn handle(shared: &Shared, cmds: &mpsc::Sender<Command>, req: Request) -> Response {
    let cmd = match req {
        Request::Snapshot => return Response::Snapshot(shared.lock().unwrap().snapshot()),
        Request::Events { run, from } => return Response::Events(shared.lock().unwrap().events(&run, from)),
        Request::Status => return Response::Status(shared.lock().unwrap().status()),
        Request::RunNow { skill } => {
            if !shared.lock().unwrap().skills.iter().any(|v| v.skill.name == skill) {
                return Response::Error(format!("no skill {skill}"));
            }
            Command::RunNow(skill)
        }
        Request::Reload => Command::Reload,
        Request::DismissFailure { run } => {
            let mut st = shared.lock().unwrap();
            if !st.dismiss_failure(&run) {
                return Response::Error(format!("no failed run {run}"));
            }
            return save_alerts(&st);
        }
        Request::Dismiss { ref skill, ref key } | Request::Notified { ref skill, ref key } => {
            let mut st = shared.lock().unwrap();
            let (done, what) = match req {
                Request::Dismiss { .. } => (st.dismiss(skill, key), "alert"),
                _ => (st.claim_notify(skill, key), "pending notification"),
            };
            if !done {
                return Response::Error(format!("no {what} {skill}/{key}"));
            }
            return save_alerts(&st);
        }
    };
    match cmds.send(cmd).await {
        Ok(()) => Response::Ok,
        Err(_) => Response::Error("scheduler stopped".into()),
    }
}

fn save_alerts(st: &AppState) -> Response {
    match (Store { dir: st.store_dir.clone() }).save_alerts(st) {
        Ok(()) => Response::Ok,
        Err(e) => Response::Error(format!("save alerts: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        let r: Request = serde_json::from_str(r#"{"type":"Events","run":"x","from":3}"#).unwrap();
        assert!(matches!(r, Request::Events { ref run, from: 3 } if run == "x"));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"type":"Status"}"#).unwrap(), Request::Status));
        let d: Request = serde_json::from_str(r#"{"type":"Dismiss","skill":"s","key":"k"}"#).unwrap();
        assert!(matches!(d, Request::Dismiss { ref skill, ref key } if skill == "s" && key == "k"));
        let n: Request = serde_json::from_str(r#"{"type":"Notified","skill":"s","key":"k"}"#).unwrap();
        assert!(matches!(n, Request::Notified { ref skill, ref key } if skill == "s" && key == "k"));
        let f: Request = serde_json::from_str(r#"{"type":"DismissFailure","run":"r"}"#).unwrap();
        assert!(matches!(f, Request::DismissFailure { ref run } if run == "r"));
        assert_eq!(serde_json::to_string(&Response::Ok).unwrap(), r#"{"type":"Ok"}"#);
        assert_eq!(
            serde_json::to_string(&Response::Events(vec![RunEvent::Text("hi".into())])).unwrap(),
            r#"{"type":"Events","data":[{"Text":"hi"}]}"#
        );
    }

    #[test]
    fn failures_are_current_and_undismissed() {
        let run = |id: &str, skill: &str, ok: bool, h: i64| RunMeta {
            id: id.into(),
            skill: skill.into(),
            runner: "claude".into(),
            model: None,
            effort: None,
            trigger_key: "k".into(),
            cwd: Default::default(),
            started_at: Local::now() - chrono::Duration::hours(h),
            finished_at: None,
            status: if ok { RunStatus::Succeeded } else { RunStatus::Failed("x".into()) },
            summary: None,
            report: None,
            verdict: None,
            exit: None,
            command: None,
            stderr_tail: vec![],
            cost_usd: None,
            input_tokens: 0,
            output_tokens: 0,
        };
        let ids = |s: &Snapshot| s.failures().iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        // Newest first: a fixed after a failure; b failed twice; c failed 2 days ago.
        let mut st = Snapshot {
            runs: vec![
                run("a2", "a", true, 1),
                run("b2", "b", false, 2),
                run("a1", "a", false, 3),
                run("b1", "b", false, 4),
                run("c1", "c", false, 48),
            ],
            ..Default::default()
        };
        assert_eq!(ids(&st), vec!["b2"]);
        st.runs.insert(0, run("b3", "b", true, 0));
        st.runs[0].status = RunStatus::Running;
        assert_eq!(ids(&st), vec!["b2"], "a running run doesn't hide the last failure");
        st.dismissed_failures.insert("b".into(), "b2".into());
        assert!(ids(&st).is_empty(), "dismissed");
        st.runs[0].status = RunStatus::Failed("y".into());
        assert_eq!(ids(&st), vec!["b3"], "newer failure shows again");
    }

    #[test]
    fn dismiss_failure_only_failed_runs() {
        let mut st = AppState::default();
        assert!(!st.dismiss_failure("nope"));
        let r: RunMeta = serde_json::from_str(
            r#"{"id":"r","skill":"s","runner":"c","model":null,"trigger_key":"k","started_at":"2026-01-01T00:00:00Z",
                "finished_at":null,"status":"Succeeded","cost_usd":null,"input_tokens":0,"output_tokens":0}"#,
        )
        .unwrap();
        st.runs.push(crate::run::Run { meta: r, events: vec![] });
        assert!(!st.dismiss_failure("r"), "succeeded run");
        st.runs[0].meta.status = RunStatus::Failed("x".into());
        assert!(st.dismiss_failure("r") && st.dismissed_failures.get("s").map(String::as_str) == Some("r"));
    }

    #[test]
    fn commit_optional() {
        let st = r#"{"root":"/x","pid":1,"running":0,"skills":0,"last_failure":null,"next_fire":null,"runner_usage":[],"power":null}"#;
        assert_eq!(serde_json::from_str::<Status>(st).unwrap().commit, "");
        let st = st.replace(r#""power":null"#, r#""power":null,"commit":"abc1234""#);
        assert_eq!(serde_json::from_str::<Status>(&st).unwrap().commit, "abc1234");
        let snap = serde_json::to_value(Snapshot { commit: "abc1234".into(), ..Default::default() }).unwrap();
        assert_eq!(serde_json::from_value::<Snapshot>(snap.clone()).unwrap().commit, "abc1234");
        let mut old = snap;
        old.as_object_mut().unwrap().remove("commit");
        assert_eq!(serde_json::from_value::<Snapshot>(old).unwrap().commit, "");
    }
}
