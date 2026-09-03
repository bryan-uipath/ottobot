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
    pub trigger_key: String,
    /// Directory the agent runs in (worktree once slice 3 lands, skills root today).
    #[serde(default)]
    pub cwd: std::path::PathBuf,
    pub started_at: DateTime<Local>,
    pub finished_at: Option<DateTime<Local>>,
    pub status: RunStatus,
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
