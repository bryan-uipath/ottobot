// TODO: duplicated from src/client.rs + src/proto.rs; move to a shared ottobot-proto crate (follow-up PR).
//! Daemon socket client: NDJSON over ~/.ottobot/otto.sock, one request line → one response line.
//! Types re-declare the subset of the root crate's proto.rs the tray reads; unknown fields are ignored.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub const OLD_PROTOCOL: &str = "daemon speaks an older protocol — restart it";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);

pub fn socket_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home dir")?.join(".ottobot").join("otto.sock"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub id: String,
    pub skill: String,
    pub runner: String,
    pub model: Option<String>,
    pub started_at: DateTime<Local>,
    pub finished_at: Option<DateTime<Local>>,
    pub status: RunStatus,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

pub type RunnerUsage = Vec<(String, Result<Vec<UsageWindow>, String>)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSnap {
    pub name: String,
    pub runner: Option<String>,
    pub model: Option<String>,
    pub trigger: Result<String, String>,
    pub next_fire: Option<DateTime<Local>>,
    pub last_fired: Option<DateTime<Local>>,
    pub active_run: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: PathBuf,
    pub skills: Vec<SkillSnap>,
    pub skill_errors: Vec<(PathBuf, String)>,
    pub runs: Vec<RunMeta>,
    pub runner_health: Vec<(String, Result<String, String>)>,
    pub runner_usage: RunnerUsage,
    pub daemon_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Snapshot,
    RunNow { skill: String },
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Response {
    Snapshot(Snapshot),
    Ok,
    Error(String),
}

/// One open socket connection, reused across requests.
pub struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Conn {
    pub async fn connect(path: &Path) -> Result<Self> {
        let s = tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(path)).await.context("connect timed out")??;
        let (r, w) = s.into_split();
        Ok(Self { reader: BufReader::new(r), writer: w })
    }

    pub async fn request(&mut self, req: &Request) -> Result<Response> {
        let io = async {
            let mut line = serde_json::to_string(req)?;
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await?;
            let mut buf = String::new();
            if self.reader.read_line(&mut buf).await? == 0 {
                bail!("daemon closed connection");
            }
            // Connected but unparseable: a daemon from another build (mirrors `Conn::find`).
            serde_json::from_str(&buf).context(OLD_PROTOCOL)
        };
        tokio::time::timeout(REQUEST_TIMEOUT, io).await.context("daemon timed out")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        assert_eq!(serde_json::to_string(&Request::RunNow { skill: "x".into() }).unwrap(), r#"{"type":"RunNow","skill":"x"}"#);
        assert!(matches!(serde_json::from_str(r#"{"type":"Ok"}"#).unwrap(), Response::Ok));
        let s = r#"{"type":"Snapshot","data":{"root":"/x","store_dir":"/s","skills":[],"skill_errors":[],"runs":[],"log":[],"daemon_pid":1,
            "runner_health":[["claude",{"Ok":"2.1"}]],
            "runner_usage":[["claude",{"Ok":[{"label":"5h","used_percent":37.0,"resets_at":null}]}],["codex",{"Err":"no sessions"}]]}}"#;
        assert!(matches!(serde_json::from_str(s).unwrap(), Response::Snapshot(Snapshot { daemon_pid: Some(1), .. })));
    }
}
