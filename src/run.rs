//! A run is one runner invocation for one skill.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub id: String,
    pub skill: String,
    pub runner: String,
    pub model: Option<String>,
    /// Reasoning effort passed to the runner, when a candidate carried `@effort`.
    #[serde(default)]
    pub effort: Option<String>,
    pub trigger_key: String,
    /// Directory the agent runs in (worktree once slice 3 lands, skills root today).
    #[serde(default)]
    pub cwd: std::path::PathBuf,
    pub started_at: DateTime<Local>,
    pub finished_at: Option<DateTime<Local>>,
    pub status: RunStatus,
    /// First line of the runner's final message; shown as the skill's "latest run" one-liner.
    #[serde(default)]
    pub summary: Option<String>,
    /// Report file: the agent's `.html`/`.md`, or ottobot's stub `.md`. Set when the run finishes.
    #[serde(default)]
    pub report: Option<std::path::PathBuf>,
    /// The agent's `$OTTO_VERDICT`, validated. None = unassessed (missing or invalid), never "normal".
    #[serde(default)]
    pub verdict: Option<Verdict>,
    pub cost_usd: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Normal,
    Critical,
    Immediate,
}

/// Severity the agent declares in `$OTTO_VERDICT` (`<store>/reports/<id>.verdict.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verdict {
    pub level: Level,
    #[serde(default, deserialize_with = "null_default")]
    pub incident_key: String,
    #[serde(default, deserialize_with = "null_default")]
    pub headline: String,
    #[serde(default, deserialize_with = "null_default")]
    pub summary: String,
    #[serde(default, deserialize_with = "null_default")]
    pub evidence: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub action: String,
}

/// Agents write `"action": null` for fields that don't apply; treat it like a missing key.
fn null_default<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

impl Verdict {
    /// Unknown level or bad JSON → None. Headline capped at 80 chars; `immediate` without evidence → `critical`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut v: Verdict = serde_json::from_str(s).ok()?;
        v.headline = crate::runner::truncate(&v.headline, 79); // + "…" = 80
        v.evidence.retain(|e| !e.trim().is_empty());
        if v.level == Level::Immediate && v.evidence.is_empty() {
            v.level = Level::Critical;
        }
        Some(v)
    }
}

/// Runner-agnostic event stream rendered by the session view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunEvent {
    Init { session: String, model: Option<String> },
    Text(String),
    ToolUse { name: String, summary: String },
    ToolResult { summary: String, is_error: bool },
    Usage { input: u64, output: u64, cost_usd: Option<f64> },
    Stderr(String),
    Raw(String),
    Done { ok: bool, summary: String },
}

impl RunMeta {
    /// `runner/model@effort`, as much as is known.
    pub fn agent_label(&self) -> String {
        format!(
            "{}{}{}",
            self.runner,
            self.model.as_ref().map(|m| format!("/{m}")).unwrap_or_default(),
            self.effort.as_ref().map(|e| format!("@{e}")).unwrap_or_default()
        )
    }

    /// One-liner under the skill: verdict headline, else the cleaned summary.
    pub fn line(&self) -> Option<&str> {
        self.verdict.as_ref().map(|v| v.headline.as_str()).filter(|h| !h.is_empty()).or(self.summary.as_deref())
    }

    pub fn status_text(&self) -> String {
        match &self.status {
            RunStatus::Succeeded => "succeeded".to_string(),
            RunStatus::Failed(why) => format!("failed: {why}"),
            RunStatus::Running => "running".to_string(),
        }
    }
}

/// First meaningful line of the agent's final message as plain text; `[Report saved](/p)` lines are skipped.
pub fn one_liner(msg: &str) -> Option<String> {
    const BOILERPLATE: [&str; 4] = ["report saved", "report written", "saved report", "wrote report"];
    msg.lines()
        .map(strip_markdown)
        .find(|l| !l.is_empty() && !BOILERPLATE.iter().any(|b| l.to_lowercase().starts_with(b)))
        .map(|l| crate::runner::truncate(&l, 200))
}

/// "## **Done** `x` [PR](u)" → "Done x PR".
fn strip_markdown(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find('[') {
        let Some(j) = rest[i..].find("](").map(|j| i + j) else { break };
        let Some(k) = rest[j..].find(')').map(|k| j + k) else { break };
        out.push_str(&rest[..i]);
        out.push_str(&rest[i + 1..j]);
        rest = &rest[k + 1..];
    }
    out.push_str(rest);
    out.replace(['`'], "").replace("**", "").trim_start_matches(['#', '-', '*', '>', ' ']).trim().to_string()
}

#[derive(Debug, Clone)]
pub struct Run {
    pub meta: RunMeta,
    pub events: Vec<RunEvent>,
}

impl Run {
    pub fn new_id(skill: &str) -> String {
        format!("{}-{}", Local::now().format("%Y%m%d-%H%M%S"), skill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_liner_cleans_markdown_and_skips_boilerplate() {
        assert_eq!(
            one_liner("[Report saved](/Users/x/r.html)\n\nChecked 20 PRs; all `mergeable`."),
            Some("Checked 20 PRs; all mergeable.".into())
        );
        assert_eq!(one_liner("[Report written](</a b/r.html>)"), None);
        assert_eq!(one_liner("Report saved to /x/r.html.\n- **3** jobs failed"), Some("3 jobs failed".into()));
        assert_eq!(one_liner("## 2 failures — [HTML report](/r.html) [x"), Some("2 failures — HTML report [x".into()));
        assert_eq!(one_liner("  \n"), None);
    }

    #[test]
    fn line_prefers_headline() {
        let mut m: RunMeta = serde_json::from_str(
            r#"{"id":"r","skill":"s","runner":"claude","model":null,"trigger_key":"k","started_at":"2026-01-01T00:00:00Z",
                "finished_at":null,"status":"Succeeded","summary":"sum","cost_usd":null,"input_tokens":0,"output_tokens":0}"#,
        )
        .unwrap();
        assert_eq!(m.line(), Some("sum"));
        m.verdict = Verdict::parse(r#"{"level":"normal","headline":""}"#);
        assert_eq!(m.line(), Some("sum"), "empty headline falls back");
        m.verdict = Verdict::parse(r#"{"level":"normal","headline":"all green"}"#);
        assert_eq!(m.line(), Some("all green"));
    }
}
