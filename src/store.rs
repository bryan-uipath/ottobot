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
    /// Event triggers: when the checker last ran, and consecutive script failures.
    #[serde(default)]
    pub last_checked_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub checker_failures: u32,
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
        fs::create_dir_all(dir.join("reports"))?;
        fs::write(dir.join("root"), root.to_string_lossy().as_bytes())?;
        Ok(Self { dir })
    }

    /// One scheduler per store: a daemon and an in-process TUI on the same root would double-fire.
    pub fn lock_scheduler(&self) -> Result<FileLock> {
        FileLock::acquire(&self.dir.join("scheduler.pid")).map_err(|e| anyhow::anyhow!("{e:#}; use the TUI as a client or stop it"))
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
    /// `<store>/reports/<id>.html`; the agent may write `.md` beside it instead.
    pub fn report_path(&self, id: &str) -> PathBuf {
        self.dir.join("reports").join(format!("{id}.html"))
    }

    /// Every finished run has a report: the agent's .html, else its .md, else a stub ottobot writes.
    pub fn finalize_report(&self, m: &mut RunMeta) {
        let html = self.report_path(&m.id);
        let md = html.with_extension("md");
        m.report = if html.is_file() {
            Some(html)
        } else if md.is_file() {
            Some(md)
        } else {
            stub_report(&md, m).ok().map(|_| md)
        };
    }

    /// Run metadata only, newest first. Runs left `Running` by a crash are finalized here as failed.
    pub fn load_metas(&self) -> Vec<RunMeta> {
        let Ok(rd) = fs::read_dir(self.dir.join("runs")) else { return vec![] };
        let mut metas: Vec<RunMeta> = rd
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| {
                let mut meta: RunMeta = serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok()?;
                if meta.status == crate::run::RunStatus::Running {
                    meta.status = crate::run::RunStatus::Failed("interrupted (ottobot exited)".into());
                    meta.finished_at = meta.finished_at.or(Some(Local::now()));
                    self.finalize_report(&mut meta);
                    let _ = self.save_run_meta(&meta);
                }
                Some(meta)
            })
            .collect();
        metas.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        metas
    }

    /// Newest first, with events.
    pub fn load_runs(&self, limit: usize) -> Vec<Run> {
        self.load_metas()
            .into_iter()
            .take(limit)
            .map(|meta| {
                let events = fs::read_to_string(self.dir.join("runs").join(format!("{}.jsonl", meta.id)))
                    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
                    .unwrap_or_default();
                Run { meta, events }
            })
            .collect()
    }
}

/// Minimal markdown report when the agent wrote none: status, model, and its final message.
fn stub_report(path: &Path, m: &RunMeta) -> std::io::Result<()> {
    use crate::run::RunStatus;
    let status = match &m.status {
        RunStatus::Succeeded => "succeeded".to_string(),
        RunStatus::Failed(why) => format!("failed: {why}"),
        RunStatus::Running => "running".to_string(),
    };
    let body = format!(
        "# {} · {}\n\n- run: `{}`\n- status: {status}\n- runner: {}{}\n- started: {}\n\n{}\n",
        m.skill,
        m.started_at.format("%Y-%m-%d %H:%M"),
        m.id,
        m.runner,
        m.model.as_ref().map(|x| format!("/{x}")).unwrap_or_default(),
        m.started_at.to_rfc3339(),
        m.summary.as_deref().unwrap_or("_the agent left no final message_"),
    );
    fs::write(path, body)
}

/// Exclusive `flock` held for the process lifetime; the kernel releases it on any exit,
/// so a crash never leaves a stale lock and two starters can't both win.
pub struct FileLock {
    _file: fs::File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        // SAFETY: valid open fd; flock has no memory-safety preconditions.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let holder = fs::read_to_string(path).unwrap_or_default();
            anyhow::bail!("{} is held by pid {}", path.display(), holder.trim());
        }
        // Pid is informational only; the flock is the lock.
        let mut f = &file;
        let _ = f.set_len(0);
        let _ = write!(f, "{}", std::process::id());
        Ok(Self { _file: file })
    }
}
