//! Daemon wire protocol and the snapshot DTO the TUI draws from.
//! Newline-delimited JSON over a Unix socket: one `Request` line → one `Response` line.

use crate::run::{RunEvent, RunMeta, RunStatus};
use crate::scheduler::Command;
use crate::state::{AppState, Shared};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

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
    pub runner_usage: Vec<(String, Result<Vec<crate::runner::UsageWindow>, String>)>,
    /// Set when served by a daemon; None in-process.
    pub daemon_pid: Option<u32>,
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
    pub runner_usage: Vec<(String, Result<Vec<crate::runner::UsageWindow>, String>)>,
}

/// e.g. `{"type":"Events","run":"20260101-120000-x","from":0}`, `{"type":"Status"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Snapshot,
    /// Events of one run starting at index `from` (the count the client already has).
    Events { run: String, from: usize },
    RunNow { skill: String },
    Reload,
    Status,
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
                    model: v.skill.front.model.clone(),
                    trigger: v.trigger.as_ref().map(|t| t.summary()).map_err(Clone::clone),
                    next_fire: v.next_fire,
                    last_fired: v.last_fired,
                    active_run: v.active_run.clone(),
                    note: v.note.clone(),
                })
                .collect(),
            skill_errors: self.skill_errors.clone(),
            runs: self.runs.iter().map(|r| r.meta.clone()).collect(),
            log: self.log.clone(),
            last_tick: self.last_tick,
            next_tick: self.next_tick,
            caffeinated: self.caffeinated,
            runner_health: self.runner_health.clone(),
            runner_usage: self.runner_usage.clone(),
            daemon_pid: self.daemon_pid,
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
            last_failure: self.runs.iter().find(|r| matches!(r.meta.status, RunStatus::Failed(_))).map(|r| (r.meta.skill.clone(), r.meta.id.clone())),
            next_fire: self.skills.iter().filter_map(|v| v.next_fire).min(),
            runner_usage: self.runner_usage.clone(),
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
    };
    match cmds.send(cmd).await {
        Ok(()) => Response::Ok,
        Err(_) => Response::Error("scheduler stopped".into()),
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
        assert_eq!(serde_json::to_string(&Response::Ok).unwrap(), r#"{"type":"Ok"}"#);
        assert_eq!(serde_json::to_string(&Response::Events(vec![RunEvent::Text("hi".into())])).unwrap(), r#"{"type":"Events","data":[{"Text":"hi"}]}"#);
    }
}
