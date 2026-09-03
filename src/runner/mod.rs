//! Runner abstraction: how to invoke an agent CLI and parse its stream.

pub mod claude;

use crate::run::RunEvent;
use anyhow::{bail, Result};
use std::path::Path;
use tokio::process::Command;

pub struct Invocation {
    pub prompt: String,
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

pub trait Runner: Send + Sync {
    fn name(&self) -> &'static str;
    fn command(&self, inv: &Invocation) -> Command;
    /// One stdout line -> zero or more normalized events.
    fn parse_line(&self, line: &str) -> Vec<RunEvent>;
    /// Binary present and usable. Returns a version string.
    fn preflight(&self) -> Result<String>;
}

pub fn by_name(name: &str) -> Result<Box<dyn Runner>> {
    match name {
        "claude" => Ok(Box::new(claude::Claude)),
        other => bail!("unknown runner `{other}` (supported: claude)"),
    }
}

pub fn all() -> Vec<Box<dyn Runner>> {
    vec![Box::new(claude::Claude)]
}

pub(crate) fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| Path::new(p).is_file())
}
