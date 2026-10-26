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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
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
                    let mut c = Conn::connect(&r.path).await.context("daemon disconnected")?;
                    let s = c.status().await.context("daemon disconnected")?;
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
