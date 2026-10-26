//! Runner abstraction: how to invoke an agent CLI and parse its stream.

pub mod claude;
pub mod codex;

use crate::run::RunEvent;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use tokio::process::Command;

pub struct Invocation {
    pub prompt: String,
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

/// One rate-limit window, e.g. claude's 5h session or codex's 7d.
#[derive(Debug, Clone)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub windows: Vec<UsageWindow>,
}

impl Usage {
    /// Remaining capacity, bounded by the tightest window. 100 when nothing is reported.
    pub fn headroom(&self) -> f64 {
        100.0 - self.windows.iter().map(|w| w.used_percent).fold(0.0, f64::max)
    }

    pub fn summary(&self) -> String {
        if self.windows.is_empty() {
            return "no usage data".into();
        }
        self.windows.iter().map(|w| format!("{} {:.0}%", w.label, w.used_percent)).collect::<Vec<_>>().join(" · ")
    }
}

pub trait Runner: Send + Sync {
    fn name(&self) -> &'static str;
    fn command(&self, inv: &Invocation) -> Command;
    /// One stdout line -> zero or more normalized events.
    fn parse_line(&self, line: &str) -> Vec<RunEvent>;
    /// Binary present and usable. Returns a version string.
    fn preflight(&self) -> Result<String>;
    /// Current quota consumption, so a caller can route work to the runner with headroom.
    fn usage(&self) -> Result<Usage>;
}

pub fn by_name(name: &str) -> Result<Box<dyn Runner>> {
    match name {
        "claude" => Ok(Box::new(claude::Claude)),
        "codex" => Ok(Box::new(codex::Codex::default())),
        other => bail!("unknown runner `{other}` (supported: claude, codex)"),
    }
}

pub fn all() -> Vec<Box<dyn Runner>> {
    vec![Box::new(claude::Claude), Box::new(codex::Codex::default())]
}

pub(crate) fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| Path::new(p).is_file())
}
