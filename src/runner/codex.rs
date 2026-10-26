//! `codex exec --json` runner. Quota comes from the newest session rollout's `rate_limits`.

use super::{which, Invocation, Runner, Usage, UsageWindow};
use crate::run::RunEvent;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::Mutex;
use tokio::process::Command;

#[derive(Default)]
pub struct Codex {
    /// Codex has no final "result" event; the last agent_message is the run summary.
    last_message: Mutex<String>,
}

impl Runner for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn command(&self, inv: &Invocation) -> Command {
        let mut c = Command::new("codex");
        c.arg("exec").args(["--json", "--skip-git-repo-check"]).arg("-C").arg(&inv.cwd);
        // bypassPermissions (claude vocabulary, our default) maps to no sandbox; anything else is a codex sandbox mode.
        match inv.permission_mode.as_deref() {
            None | Some("bypassPermissions") => {
                c.arg("--dangerously-bypass-approvals-and-sandbox");
            }
            Some(mode) => {
                c.args(["-s", mode]);
            }
        }
        if let Some(m) = &inv.model {
            c.args(["-m", m]);
        }
        c.arg(&inv.prompt);
        c
    }

    fn parse_line(&self, line: &str) -> Vec<RunEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return if line.trim().is_empty() { vec![] } else { vec![RunEvent::Raw(line.to_string())] };
        };
        let item = &v["item"];
        match v["type"].as_str() {
            Some("thread.started") => vec![RunEvent::Init { session: v["thread_id"].as_str().unwrap_or("").to_string(), model: None }],
            Some("item.started") => match item["type"].as_str() {
                Some("command_execution") => vec![RunEvent::ToolUse { name: "shell".into(), summary: truncate(item["command"].as_str().unwrap_or(""), 160) }],
                Some("mcp_tool_call") => vec![RunEvent::ToolUse {
                    name: format!("{}/{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("?")),
                    summary: truncate(&item["arguments"].to_string(), 160),
                }],
                Some("web_search") => vec![RunEvent::ToolUse { name: "web_search".into(), summary: truncate(item["query"].as_str().unwrap_or(""), 160) }],
                _ => vec![],
            },
            Some("item.completed") => match item["type"].as_str() {
                Some("agent_message") => {
                    let t = item["text"].as_str().unwrap_or("").to_string();
                    *self.last_message.lock().unwrap() = t.clone();
                    if t.trim().is_empty() { vec![] } else { vec![RunEvent::Text(t)] }
                }
                Some("command_execution") => vec![RunEvent::ToolResult {
                    summary: truncate(item["aggregated_output"].as_str().unwrap_or(""), 300),
                    is_error: item["exit_code"].as_i64().unwrap_or(0) != 0,
                }],
                Some("mcp_tool_call") => vec![RunEvent::ToolResult {
                    summary: truncate(&item["result"].to_string(), 300),
                    is_error: item["status"].as_str() == Some("failed"),
                }],
                Some("file_change") => vec![RunEvent::ToolUse {
                    name: "edit".into(),
                    summary: truncate(
                        &item["changes"].as_array().map(|a| a.iter().filter_map(|c| c["path"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                        160,
                    ),
                }],
                Some("error") => vec![RunEvent::Stderr(item["message"].as_str().unwrap_or("").to_string())],
                _ => vec![],
            },
            Some("turn.completed") => {
                let u = &v["usage"];
                vec![
                    RunEvent::Usage { input: u["input_tokens"].as_u64().unwrap_or(0), output: u["output_tokens"].as_u64().unwrap_or(0), cost_usd: None },
                    RunEvent::Done { ok: true, summary: self.last_message.lock().unwrap().clone() },
                ]
            }
            Some("turn.failed") => vec![RunEvent::Done { ok: false, summary: v["error"]["message"].as_str().unwrap_or("turn failed").to_string() }],
            Some("error") => vec![RunEvent::Stderr(v["message"].as_str().unwrap_or("").to_string())],
            _ => vec![],
        }
    }

    fn preflight(&self) -> Result<String> {
        which("codex").context("`codex` not on PATH")?;
        let out = std::process::Command::new("codex").arg("--version").output().context("codex --version")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Codex exposes no usage API; every session rollout logs `rate_limits` on each token_count event.
    fn usage(&self) -> Result<Usage> {
        let sessions = dirs::home_dir().context("no home")?.join(".codex/sessions");
        let newest = walkdir::WalkDir::new(&sessions)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.into_path())))
            .max_by_key(|(t, _)| *t)
            .map(|(_, p)| p)
            .context("no codex sessions")?;
        let text = std::fs::read_to_string(&newest)?;
        let rl = text
            .lines()
            .rev()
            .filter(|l| l.contains("rate_limits"))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| v["payload"]["rate_limits"].clone())
            .find(|r| !r.is_null())
            .with_context(|| format!("no rate_limits in {}", newest.display()))?;
        let mut windows = vec![];
        for key in ["primary", "secondary"] {
            let w = &rl[key];
            if w.is_null() {
                continue;
            }
            let mins = w["window_minutes"].as_u64().unwrap_or(0);
            windows.push(UsageWindow {
                label: if mins >= 1440 { format!("{}d", mins / 1440) } else { format!("{}h", mins / 60) },
                used_percent: w["used_percent"].as_f64().unwrap_or(0.0),
                resets_at: w["resets_at"].as_i64().and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
            });
        }
        Ok(Usage { windows })
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim().replace('\n', " ⏎ ");
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}
