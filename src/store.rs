//! Persistent state under ~/.ottobot/<hash-of-skills-root>/.

use crate::run::{Level, Run, RunEvent, RunMeta, Verdict};
use crate::state::Alert;
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

    /// Separate from state.json: only written under the shared-state lock, so it never races the scheduler's state saves.
    pub fn load_alerts(&self) -> Vec<Alert> {
        fs::read_to_string(self.dir.join("alerts.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save_alerts(&self, a: &[Alert]) -> Result<()> {
        let tmp = self.dir.join("alerts.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(a)?)?;
        fs::rename(tmp, self.dir.join("alerts.json"))?;
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
        m.verdict = fs::read_to_string(html.with_extension("verdict.json")).ok().and_then(|s| Verdict::parse(&s));
        // An agent-written page wins. A page *we* rendered is derived: re-render if the .md is there, so a
        // stub written during crash reconciliation never hides the agent's real notes.
        let ours = fs::read_to_string(&html).map(|s| s.starts_with(RENDERED_MARK)).unwrap_or(false);
        if html.is_file() && !ours {
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
fn render_markdown(md: &Path, html: &Path, m: &RunMeta) -> std::io::Result<()> {
    use pulldown_cmark::{html::push_html, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
    let title = format!("{} · {}", m.skill, m.started_at.format("%Y-%m-%d %H:%M"));
    let src = fs::read_to_string(md)?;
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // Raw HTML becomes text (reports quote untrusted logs/Slack); bare URLs in text become links,
    // except inside an existing link or a code span.
    let mut in_link = false;
    let mut in_code = false;
    let mut events: Vec<Event> = Vec::new();
    for ev in Parser::new_ext(&src, opts) {
        match ev {
            Event::Html(t) | Event::InlineHtml(t) => events.push(Event::Text(t)),
            Event::Start(Tag::Link { .. }) => {
                in_link = true;
                events.push(ev);
            }
            Event::End(TagEnd::Link) => {
                in_link = false;
                events.push(ev);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                events.push(ev);
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                events.push(ev);
            }
            Event::Text(t) if !in_link && !in_code && (t.contains("http://") || t.contains("https://")) => {
                for piece in split_urls(&t) {
                    match piece {
                        Ok(url) => {
                            let u: CowStr = url.to_string().into();
                            events.push(Event::Start(Tag::Link {
                                link_type: LinkType::Autolink,
                                dest_url: u.clone(),
                                title: "".into(),
                                id: "".into(),
                            }));
                            events.push(Event::Text(u));
                            events.push(Event::End(TagEnd::Link));
                        }
                        Err(text) => events.push(Event::Text(text.to_string().into())),
                    }
                }
            }
            other => events.push(other),
        }
    }
    let mut body = String::new();
    push_html(&mut body, events.into_iter());
    let page = format!(
        "{RENDERED_MARK}<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\">\
         <title>{title}</title><style>{REPORT_CSS}{VERDICT_CSS}</style><main>{meta}{verdict}{body}</main>",
        title = html_escape(&title),
        meta = meta_block(m),
        verdict = verdict_block(m),
    );
    fs::write(html, page)
}

const RENDERED_MARK: &str = "<!--otto-rendered-->";

/// Split text into plain pieces (Err) and bare http(s) URLs (Ok). Trailing `.,;:` stay text;
/// a closing `)` ends the URL only when unbalanced.
fn split_urls(text: &str) -> Vec<Result<&str, &str>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        if i > 0 {
            out.push(Err(&rest[..i]));
        }
        let tail = &rest[i..];
        let mut end = tail.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"' || c == '\'').unwrap_or(tail.len());
        let mut url = &tail[..end];
        // Balance parentheses, then drop trailing punctuation.
        while url.ends_with(')') && url.matches('(').count() < url.matches(')').count() {
            url = &url[..url.len() - 1];
        }
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        end = trimmed.len();
        out.push(Ok(trimmed));
        rest = &tail[end..];
    }
    if !rest.is_empty() {
        out.push(Err(rest));
    }
    out
}

/// Who ran this: skill, run id, status, runner/model@effort, duration, cost. Same text for both report kinds.
fn meta_block(m: &RunMeta) -> String {
    let dur = m.finished_at.map(|f| (f - m.started_at).num_seconds().max(0)).unwrap_or(0);
    let cost = m.cost_usd.map(|c| format!(" · ${c:.2}")).unwrap_or_default();
    let tokens = if m.input_tokens + m.output_tokens > 0 {
        format!(" · {} in / {} out", kfmt(m.input_tokens), kfmt(m.output_tokens))
    } else {
        String::new()
    };
    format!(
        "<!--otto-meta--><div class=\"otto-meta\"><b>ottobot</b> · {skill} · <code>{id}</code> · {status} · agent <b>{agent}</b> · {mm}m{ss:02}s{tokens}{cost} · {started}</div>",
        skill = html_escape(&m.skill),
        id = html_escape(&m.id),
        status = html_escape(&m.status_text()),
        agent = html_escape(&m.agent_label()),
        mm = dur / 60,
        ss = dur % 60,
        started = m.started_at.format("%Y-%m-%d %H:%M %:z"),
    )
}

/// TL;DR banner: a boxed summary for critical/immediate, one dim line for normal, nothing when unassessed.
/// Every field is LLM-written from untrusted telemetry, so all of it is escaped.
fn verdict_block(m: &RunMeta) -> String {
    let Some(v) = &m.verdict else { return String::new() };
    let (class, label) = match v.level {
        Level::Normal => return format!("<div class=\"otto-normal\">normal · {}</div>", html_escape(&v.headline)),
        Level::Critical => ("otto-critical", "CRITICAL"),
        Level::Immediate => ("otto-immediate", "IMMEDIATE"),
    };
    let evidence: String = v.evidence.iter().map(|e| format!("<li>{}</li>", html_escape(e))).collect();
    let action = if v.action.is_empty() { String::new() } else { format!("<p><b>Action:</b> {}</p>", html_escape(&v.action)) };
    format!(
        "<div class=\"otto-verdict {class}\"><p><b>{label}</b> · {headline}</p><p>{summary}</p><ul>{evidence}</ul>{action}</div>",
        headline = html_escape(&v.headline),
        summary = html_escape(&v.summary),
    )
}

fn kfmt(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Agent-written HTML gets the same meta line appended once (before </body> when present),
/// and the verdict banner right after the opening <body> tag.
fn stamp_html(html: &Path, m: &RunMeta) -> std::io::Result<()> {
    let mut s = fs::read_to_string(html)?;
    if s.contains("<!--otto-meta-->") {
        return Ok(());
    }
    let block = format!("<style>{META_CSS}{VERDICT_CSS}</style>{}", meta_block(m));
    s = match s.rfind("</body>") {
        Some(i) => format!("{}{}{}", &s[..i], block, &s[i..]),
        None => format!("{s}{block}"),
    };
    s.insert_str(body_start(&s), &verdict_block(m));
    fs::write(html, s)
}

/// Offset just past the real `<body…>` tag (any case, not inside a comment); else after `</head>` or the doctype.
fn body_start(s: &str) -> usize {
    let l = s.to_ascii_lowercase(); // same byte offsets as `s`
    let mut i = 0;
    while let Some(j) = l[i..].find("<body") {
        let at = i + j;
        let in_comment = l[..at].rfind("<!--").is_some_and(|c| !l[c..at].contains("-->"));
        if !in_comment && l[at + 5..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace()) {
            return l[at..].find('>').map_or(s.len(), |k| at + k + 1);
        }
        i = at + 5;
    }
    l.find("</head>")
        .map(|h| h + 7)
        .or_else(|| l.trim_start().starts_with("<!doctype").then(|| l.find('>').map_or(0, |k| k + 1)))
        .unwrap_or(0)
}

// Stamped into agent pages of unknown palette: inherit their text colour, just dimmer.
const META_CSS: &str = ".otto-meta{font:12px/1.5 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;opacity:.65;border-top:1px solid currentColor;margin-top:28px;padding-top:10px}.otto-meta code{font:11px ui-monospace,Menlo,monospace}";

// Translucent fills over whatever palette the page has: yellow for critical, red for immediate.
const VERDICT_CSS: &str = ".otto-verdict{font:14px/1.5 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;border:1px solid;border-left-width:4px;border-radius:6px;padding:4px 14px;margin:12px 0 20px}.otto-verdict p{margin:8px 0}.otto-verdict ul{margin:4px 0;padding-left:1.4em}.otto-critical{border-color:#f5a623;background:rgba(245,166,35,.14)}.otto-immediate{border-color:#ff5c5c;background:rgba(255,92,92,.16)}.otto-normal{font:12px/1.5 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;opacity:.7;margin:4px 0 12px}";

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// Palette copied from tray/ui/style.css (itself from flow-presentation tokens.css); keep in sync by hand.
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
    let body = format!(
        "# {} · {}\n\n- run: `{}`\n- status: {}\n- agent: {}\n- started: {}\n\n{}\n",
        m.skill,
        m.started_at.format("%Y-%m-%d %H:%M"),
        m.id,
        m.status_text(),
        m.agent_label(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_cleanly() {
        let v = split_urls("see https://x.com/a, and (https://en.wikipedia.org/wiki/Rust_(language)) now.");
        assert_eq!(v[1], Ok("https://x.com/a"));
        assert_eq!(v[3], Ok("https://en.wikipedia.org/wiki/Rust_(language)"));
        assert_eq!(v[4], Err(") now."));
        assert_eq!(split_urls("no links"), vec![Err("no links")]);
    }

    fn meta(id: &str) -> RunMeta {
        RunMeta {
            id: id.into(),
            skill: "s".into(),
            runner: "claude".into(),
            model: None,
            effort: None,
            trigger_key: "k".into(),
            cwd: PathBuf::new(),
            started_at: Local::now(),
            finished_at: Some(Local::now()),
            status: crate::run::RunStatus::Succeeded,
            summary: None,
            report: None,
            verdict: None,
            cost_usd: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    #[test]
    fn verdict_validation() {
        assert_eq!(Verdict::parse(r#"{"level":"panic","headline":"x"}"#), None);
        assert_eq!(Verdict::parse("not json"), None);
        let v = Verdict::parse(&format!(r#"{{"level":"immediate","headline":"{}","evidence":[" "]}}"#, "h".repeat(100))).unwrap();
        assert_eq!(v.level, Level::Critical);
        assert_eq!(v.headline.chars().count(), 80); // 79 + ellipsis
        assert_eq!(Verdict::parse(r#"{"level":"immediate","evidence":["5xx 4% vs 0.1%"]}"#).unwrap().level, Level::Immediate);
    }

    #[test]
    fn verdict_nulls_are_defaults() {
        let v = Verdict::parse(r#"{"level":"critical","incident_key":null,"headline":"5xx up","evidence":null,"action":null}"#);
        assert_eq!(v.map(|v| (v.level, v.headline)), Some((Level::Critical, "5xx up".into())));
    }

    #[test]
    fn body_start_skips_comments_and_case() {
        let s = "<!doctype html><html><head><!-- <body> below --></head><BODY class=x><p>r</p></BODY></html>";
        assert!(s[body_start(s)..].starts_with("<p>r"));
        let s = "<!DOCTYPE html><html><head><title>t</title></head><p>r</p>";
        assert!(s[body_start(s)..].starts_with("<p>r"));
        let s = "<!doctype html><p>r</p>";
        assert!(s[body_start(s)..].starts_with("<p>r"));
        assert!(body_start("<bodyguard>") == 0);
    }

    #[test]
    fn banner_escapes() {
        let mut m = meta("r");
        m.verdict = Verdict::parse(r#"{"level":"critical","headline":"<script>x</script>","evidence":["<img onerror=y>"],"action":"a&b"}"#);
        let b = verdict_block(&m);
        assert!(!b.contains("<script") && !b.contains("<img"), "{b}");
        assert!(b.contains("&lt;script&gt;") && b.contains("a&amp;b") && b.contains("otto-critical"));
    }

    #[test]
    fn finalize_reads_verdict_sidecar() {
        let dir = std::env::temp_dir().join(format!("otto-verdict-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("reports")).unwrap();
        let store = Store { dir: dir.clone() };
        let mut m = meta("run1");
        fs::write(store.report_path("run1"), "<html><body><p>hi</p></body></html>").unwrap();
        fs::write(dir.join("reports/run1.verdict.json"), r#"{"level":"immediate","headline":"down","evidence":["0 rps"]}"#).unwrap();
        store.finalize_report(&mut m);
        assert_eq!(m.verdict.as_ref().map(|v| v.level), Some(Level::Immediate));
        let page = fs::read_to_string(store.report_path("run1")).unwrap();
        assert!(page.starts_with("<html><body><div class=\"otto-verdict otto-immediate\">"), "{page}");
        // No sidecar → unassessed, md path renders no banner.
        let mut m2 = meta("run2");
        store.finalize_report(&mut m2);
        assert!(m2.verdict.is_none());
        assert!(!fs::read_to_string(store.report_path("run2")).unwrap().contains("class=\"otto-verdict"));
        let _ = fs::remove_dir_all(dir);
    }
}
