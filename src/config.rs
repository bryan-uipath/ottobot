//! ~/.ottobot/config.json. Everything optional; CLI flags win.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Default skills folder when no ROOT arg is given. Default: ~/.ottobot/skills
    pub skills_root: Option<PathBuf>,
    pub runner: Option<String>,
    #[serde(default, with = "humantime_serde")]
    pub tick: Option<std::time::Duration>,
}

pub fn home() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home dir")?.join(".ottobot"))
}

pub fn load() -> Result<Config> {
    let path = home()?.join("config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {}", path.display())),
        Err(_) => Ok(Config::default()),
    }
}

/// Resolve the skills root: CLI arg, then config, then ~/.ottobot/skills (created if missing).
pub fn skills_root(cli: Option<PathBuf>, cfg: &Config) -> Result<PathBuf> {
    let root = match cli.or_else(|| cfg.skills_root.clone()) {
        Some(p) => expand_tilde(p),
        None => {
            let d = home()?.join("skills");
            std::fs::create_dir_all(&d)?;
            d
        }
    };
    root.canonicalize().with_context(|| format!("skills root {}", root.display()))
}

fn expand_tilde(p: PathBuf) -> PathBuf {
    match (p.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(h)) => h.join(rest),
        _ => p,
    }
}
