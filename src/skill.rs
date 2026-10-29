//! Skill discovery: markdown files with a `trigger:` frontmatter block.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
    pub front: Frontmatter,
    /// Hash of the whole file. Changes when anything is edited.
    pub hash: String,
    /// Hash of the trigger block only. Changing it resets dedupe/cooldown state.
    pub trigger_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub runner: Option<String>,
    /// `opus`, `codex/gpt-6-astra`, a config alias, or a list of those (most quota left wins).
    pub model: Option<ModelSpec>,
    pub checker_model: Option<String>,
    pub permission_mode: Option<String>,
    pub trigger: TriggerSpec,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelSpec {
    One(String),
    Many(Vec<String>),
}

impl ModelSpec {
    pub fn candidates(&self) -> Vec<String> {
        match self {
            ModelSpec::One(s) => vec![s.clone()],
            ModelSpec::Many(v) => v.clone(),
        }
    }

    /// `a | b` for display.
    pub fn summary(&self) -> String {
        self.candidates().join(" | ")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TriggerSpec {
    pub cron: Option<String>,
    pub event: Option<String>,
    #[serde(default, with = "humantime_serde")]
    pub check_every: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub cooldown: Option<Duration>,
}

/// Walk `root` for skills. Skips hidden dirs and node_modules. Files without a
/// `trigger:` frontmatter are ignored silently; malformed ones are returned as errors.
pub fn discover(root: &Path) -> (Vec<Skill>, Vec<(PathBuf, String)>) {
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        let n = e.file_name().to_string_lossy();
        !(e.depth() > 0 && (n.starts_with('.') || n == "node_modules" || n == "target"))
    });
    for entry in walker.filter_map(Result::ok) {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let Some((front_text, body)) = split_frontmatter(&text) else { continue };
        if !front_text.contains("trigger:") {
            continue;
        }
        match parse(p, front_text, body, &text) {
            Ok(s) => skills.push(s),
            Err(e) => errors.push((p.to_path_buf(), format!("{e:#}"))),
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    (skills, errors)
}

fn parse(path: &Path, front_text: &str, body: &str, full: &str) -> Result<Skill> {
    let front: Frontmatter = serde_yaml::from_str(front_text).context("frontmatter")?;
    if front.trigger.cron.is_none() && front.trigger.event.is_none() {
        bail!("trigger needs `cron:` or `event:`");
    }
    let trigger_yaml: serde_yaml::Value = serde_yaml::from_str(front_text)?;
    let trigger_text = serde_yaml::to_string(&trigger_yaml["trigger"])?;
    let name = front.name.clone().unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    Ok(Skill {
        name,
        path: path.to_path_buf(),
        body: body.trim().to_string(),
        front,
        hash: blake3::hash(full.as_bytes()).to_hex()[..16].to_string(),
        trigger_hash: blake3::hash(trigger_text.as_bytes()).to_hex()[..16].to_string(),
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    Some((front, body))
}
