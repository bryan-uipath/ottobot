//! TUI backend: in-process state, or a daemon over its Unix socket.

use crate::proto::{self, Request, Response, Snapshot, Status};
use crate::run::RunEvent;
use crate::scheduler::Command;
use crate::state::Shared;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

// Short: the TUI blocks on each request, so a stalled daemon must not eat keypresses.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

pub fn socket_path() -> Result<PathBuf> {
    Ok(crate::config::home()?.join("otto.sock"))
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
            Ok(serde_json::from_str(&buf)?)
        };
        tokio::time::timeout(REQUEST_TIMEOUT, io).await.context("daemon timed out")?
    }

    /// Connect and ask who is there. Any failure means "no usable daemon".
    pub async fn probe(path: &Path) -> Result<(Self, Status)> {
        let mut c = Self::connect(path).await?;
        let s = c.status().await?;
        Ok((c, s))
    }

    /// Daemon at `path`, if one accepts connections. Connected but unintelligible is an error, not "no daemon".
    pub async fn find(path: &Path) -> Result<Option<(Self, Status)>> {
        let Ok(mut c) = Self::connect(path).await else { return Ok(None) };
        match c.status().await {
            Ok(s) => Ok(Some((c, s))),
            Err(e) => bail!(
                "otto daemon at {} speaks an older protocol; restart it (otto daemon --install, or kill the pid in ~/.ottobot/daemon.json): {e:#}",
                path.display()
            ),
        }
    }

    pub async fn status(&mut self) -> Result<Status> {
        match self.request(&Request::Status).await? {
            Response::Status(s) => Ok(s),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

/// Remote side: drops the connection on error and reconnects at most every 2s.
pub struct Remote {
    path: PathBuf,
    /// Root the daemon served at connect time; a replacement daemon for another root is refused.
    root: PathBuf,
    conn: Option<Conn>,
    last_try: Instant,
}

pub enum Backend {
    Local { shared: Shared, cmds: mpsc::Sender<Command> },
    Remote(Remote),
}

impl Backend {
    pub fn remote(path: PathBuf, root: PathBuf, conn: Conn) -> Self {
        Backend::Remote(Remote { path, root, conn: Some(conn), last_try: Instant::now() })
    }

    async fn request(&mut self, req: Request) -> Result<Response> {
        let res = match self {
            Backend::Local { shared, cmds } => proto::handle(shared, cmds, req).await,
            Backend::Remote(r) => {
                if r.conn.is_none() {
                    if r.last_try.elapsed() < RECONNECT_EVERY {
                        bail!("daemon disconnected");
                    }
                    r.last_try = Instant::now();
                    let (c, s) = Conn::probe(&r.path).await.context("daemon disconnected")?;
                    if s.root != r.root {
                        bail!("daemon now serves {} (wanted {})", s.root.display(), r.root.display());
                    }
                    r.conn = Some(c);
                }
                let Some(conn) = r.conn.as_mut() else { bail!("daemon disconnected") };
                let res = conn.request(&req).await;
                if res.is_err() {
                    r.conn = None;
                    r.last_try = Instant::now();
                }
                res.context("daemon disconnected")?
            }
        };
        match res {
            Response::Error(e) => bail!(e),
            r => Ok(r),
        }
    }

    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        match self.request(Request::Snapshot).await? {
            Response::Snapshot(s) => Ok(s),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    pub async fn events(&mut self, run: &str, from: usize) -> Result<Vec<RunEvent>> {
        match self.request(Request::Events { run: run.to_string(), from }).await? {
            Response::Events(e) => Ok(e),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    pub async fn run_now(&mut self, skill: &str) -> Result<()> {
        self.request(Request::RunNow { skill: skill.to_string() }).await.map(|_| ())
    }

    pub async fn reload(&mut self) -> Result<()> {
        self.request(Request::Reload).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Relative path: absolute temp paths can exceed the AF_UNIX length limit.
    #[tokio::test]
    async fn find_distinguishes_absent_from_old_protocol() {
        let path = PathBuf::from(format!("target/find-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(Conn::find(&path).await.unwrap().is_none());

        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            // Pre-board daemon: runner_usage was a summary string.
            w.write_all(br#"{"type":"Status","data":{"root":"/x","pid":1,"running":0,"skills":0,"last_failure":null,"next_fire":null,"runner_usage":[["claude",{"Ok":"5h 3%"}]]}}"#).await.unwrap();
            w.write_all(b"\n").await.unwrap();
        });
        let err = Conn::find(&path).await.err().map(|e| e.to_string()).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("older protocol"), "{err}");
    }
}
