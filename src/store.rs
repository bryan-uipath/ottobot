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

    /// `<store>/memory/<skill>/`, created on demand. Notes, baselines, open investigations.
    pub fn memory_dir(&self, skill: &str) -> PathBuf {
        let d = self.dir.join("memory").join(skill);
        let _ = fs::create_dir_all(&d);
        d
    }

    /// `<store>/reports/<id>.html`; the agent may write `.md` beside it instead.
    pub fn report_path(&self, id: &str) -> PathBuf {
        self.dir.join("reports").join(format!("{id}.html"))
    }

    /// Every finished run has a report: the agent's .html, else its .md, else a stub ottobot writes.
    pub fn finalize_report(&self, m: &mut RunMeta) {
        let html = self.report_path(&m.id);
        let md = html.with_extension("md");
        if html.is_file() {
            let _ = stamp_html(&html, m);
            m.report = Some(html);
            return;
        }
        if !md.is_file() && stub_report(&md, m).is_err() {
            m.report = None;
            return;
        }
        // Markdown stays the source; the rendered page is what opens, so links are clickable.
        m.report = Some(match render_markdown(&md, &html, m) {
            Ok(()) => html,
            Err(_) => md,
        });
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

/// `.md` → self-contained dark-friendly `.html` (future-dark tokens). Bare URLs become links too.
pub fn render_markdown(md: &Path, html: &Path, m: &RunMeta) -> std::io::Result<()> {
    use pulldown_cmark::{html::push_html, Options, Parser};
    let title = format!("{} · {}", m.skill, m.started_at.format("%Y-%m-%d %H:%M"));
    let src = fs::read_to_string(md)?;
    let mut body = String::new();
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    push_html(&mut body, Parser::new_ext(&src, opts));
    let body = autolink(&body);
    let page = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{REPORT_CSS}</style><main>{meta}{body}</main>",
        title = html_escape(&title),
        meta = meta_block(m),
    );
    fs::write(html, page)
}

/// Who ran this: skill, run id, status, runner/model@effort, duration, cost. Same text for both report kinds.
fn meta_block(m: &RunMeta) -> String {
    use crate::run::RunStatus;
    let status = match &m.status {
        RunStatus::Succeeded => "succeeded".to_string(),
        RunStatus::Failed(why) => format!("failed: {why}"),
        RunStatus::Running => "running".to_string(),
    };
    let agent = format!(
        "{}{}{}",
        m.runner,
        m.model.as_ref().map(|x| format!("/{x}")).unwrap_or_default(),
        m.effort.as_ref().map(|e| format!("@{e}")).unwrap_or_default()
    );
    let dur = m.finished_at.map(|f| (f - m.started_at).num_seconds()).unwrap_or(0);
    let cost = m.cost_usd.map(|c| format!(" · ${c:.2}")).unwrap_or_default();
    let tokens = if m.input_tokens + m.output_tokens > 0 {
        format!(" · {}k in / {}k out", m.input_tokens / 1000, m.output_tokens / 1000)
    } else {
        String::new()
    };
    format!(
        "<!--otto-meta--><div class=\"otto-meta\"><b>ottobot</b> · {skill} · <code>{id}</code> · {status} · agent <b>{agent}</b> · {m}m{s:02}s{tokens}{cost} · {started}</div>",
        skill = html_escape(&m.skill),
        id = html_escape(&m.id),
        status = html_escape(&status),
        agent = html_escape(&agent),
        m = dur / 60,
        s = dur % 60,
        started = m.started_at.format("%Y-%m-%d %H:%M %Z"),
    )
}

/// Agent-written HTML gets the same meta line appended once (before </body> when present).
fn stamp_html(html: &Path, m: &RunMeta) -> std::io::Result<()> {
    let s = fs::read_to_string(html)?;
    if s.contains("<!--otto-meta-->") {
        return Ok(());
    }
    let block = format!("<style>{META_CSS}</style>{}", meta_block(m));
    let out = match s.rfind("</body>") {
        Some(i) => format!("{}{}{}", &s[..i], block, &s[i..]),
        None => format!("{s}{block}"),
    };
    fs::write(html, out)
}

const META_CSS: &str = ".otto-meta{font:12px/1.5 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;color:#a4b1b8;border-top:1px solid #33414c;margin-top:28px;padding-top:10px}.otto-meta code{font:11px ui-monospace,Menlo,monospace}";

/// Wrap bare http(s) URLs in text nodes with <a>; leaves existing tags alone.
fn autolink(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_anchor = false;
    let mut i = 0;
    let b = html.as_bytes();
    while i < html.len() {
        if b[i] == b'<' {
            in_tag = true;
            if html[i..].starts_with("<a ") {
                in_anchor = true;
            } else if html[i..].starts_with("</a>") {
                in_anchor = false;
            }
        }
        if !in_tag && !in_anchor && (html[i..].starts_with("http://") || html[i..].starts_with("https://")) {
            let end = html[i..]
                .find(|c: char| c.is_whitespace() || c == '<' || c == '"' || c == ')' || c == '\'')
                .map(|n| i + n)
                .unwrap_or(html.len());
            let url = html[i..end].trim_end_matches(['.', ',', ';', ':']);
            out.push_str(&format!("<a href=\"{url}\">{url}</a>"));
            i += url.len();
            continue;
        }
        if b[i] == b'>' {
            in_tag = false;
        }
        let ch = html[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

const REPORT_CSS: &str = r#"
:root{--bg:#131a20;--bg-1:#182027;--bg-2:#273139;--fg:#f4f5f7;--fg-de:#cfd8dd;--fg-light:#a4b1b8;--border:#33414c;--brand:#66adff;--err:#ff8484;--ok:#74c94b;--warn:#f5a623}
@media (prefers-color-scheme: light){:root{--bg:#eef1f4;--bg-1:#f6f8fa;--bg-2:#fff;--fg:#1b2830;--fg-de:#3d4d57;--fg-light:#5b6b75;--border:#d3dce2;--brand:#0067df;--err:#d64545;--ok:#3f9e2b;--warn:#b9791a}}
html{background:var(--bg);color:var(--fg);font:15px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif}
main{max-width:860px;margin:0 auto;padding:32px 20px 64px}
h1,h2,h3{line-height:1.25;margin:1.4em 0 .5em}h1{font-size:1.6em}h2{font-size:1.25em;border-bottom:1px solid var(--border);padding-bottom:.3em}
a{color:var(--brand);text-decoration:none}a:hover{text-decoration:underline}
code,pre{font:13px/1.45 ui-monospace,"SF Mono",Menlo,monospace;background:var(--bg-1);border:1px solid var(--border);border-radius:6px}
code{padding:1px 5px}pre{padding:12px 14px;overflow:auto}pre code{border:0;padding:0;background:none}
table{border-collapse:collapse;width:100%;margin:1em 0;font-size:14px}th,td{text-align:left;padding:6px 10px;border-bottom:1px solid var(--border);vertical-align:top}th{color:var(--fg-light);font-weight:600}
.otto-meta{font-size:12px;color:var(--fg-light);border-bottom:1px solid var(--border);padding-bottom:10px;margin-bottom:8px}.otto-meta code{font-size:11px}
blockquote{margin:1em 0;padding:.2em 1em;border-left:3px solid var(--border);color:var(--fg-de)}
ul,ol{padding-left:1.4em}li+li{margin-top:.2em}hr{border:0;border-top:1px solid var(--border);margin:2em 0}
"#;

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
