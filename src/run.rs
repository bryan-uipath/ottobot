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
    pub cost_usd: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
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

    pub fn status_text(&self) -> String {
        match &self.status {
            RunStatus::Succeeded => "succeeded".to_string(),
            RunStatus::Failed(why) => format!("failed: {why}"),
            RunStatus::Running => "running".to_string(),
        }
    }
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
