//! Persistent state under ~/.ottobot/<hash-of-skills-root>/.

use crate::run::{Run, RunEvent, RunMeta};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillState {
    pub hash: String,
    pub trigger_hash: String,
    pub last_fired_at: Option<DateTime<Local>>,
    pub last_fired_key: Option<String>,
    pub last_run_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub skills: HashMap<String, SkillState>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn open(skills_root: &Path) -> Result<Self> {
        let root = skills_root.canonicalize()?;
        // Same scheme as ~/.claude/projects: path with separators replaced by '-'.
        let key: String = root.to_string_lossy().chars().map(|c| if c == '/' || c == '\\' || c == ' ' { '-' } else { c }).collect();
        let dir = crate::config::home()?.join("projects").join(key);
        fs::create_dir_all(dir.join("runs"))?;
        fs::write(dir.join("root"), root.to_string_lossy().as_bytes())?;
        Ok(Self { dir })
    }

    pub fn load_state(&self) -> State {
        fs::read_to_string(self.dir.join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save_state(&self, s: &State) -> Result<()> {
        let tmp = self.dir.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
        fs::rename(tmp, self.dir.join("state.json"))?;
        Ok(())
    }

    pub fn save_run_meta(&self, m: &RunMeta) -> Result<()> {
        fs::write(self.dir.join("runs").join(format!("{}.json", m.id)), serde_json::to_string_pretty(m)?)?;
        Ok(())
    }

    pub fn append_event(&self, id: &str, e: &RunEvent) -> Result<()> {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join("runs").join(format!("{id}.jsonl")))?;
        writeln!(f, "{}", serde_json::to_string(e)?)?;
        Ok(())
    }

    /// Newest first. Runs left `Running` by a crash are reported as failed.
    pub fn load_runs(&self, limit: usize) -> Vec<Run> {
        let Ok(rd) = fs::read_dir(self.dir.join("runs")) else { return vec![] };
        let mut runs: Vec<Run> = rd
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| {
                let mut meta: RunMeta = serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok()?;
                if meta.status == crate::run::RunStatus::Running {
                    meta.status = crate::run::RunStatus::Failed("interrupted (ottobot exited)".into());
                }
                let events = fs::read_to_string(e.path().with_extension("jsonl"))
                    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
                    .unwrap_or_default();
                Some(Run { meta, events })
            })
            .collect();
        runs.sort_by(|a, b| b.meta.started_at.cmp(&a.meta.started_at));
        runs.truncate(limit);
        runs
    }
}
