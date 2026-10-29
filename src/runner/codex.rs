//! `codex exec --json` runner. Quota comes from the newest session rollout's `rate_limits`.

use super::{truncate, version, Invocation, Runner, Usage, UsageWindow};
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
        if let Some(e) = &inv.effort {
            c.args(["-c", &format!("model_reasoning_effort=\"{e}\"")]);
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
                Some("command_execution") => {
                    vec![RunEvent::ToolUse { name: "shell".into(), summary: truncate(item["command"].as_str().unwrap_or(""), 160) }]
                }
                Some("mcp_tool_call") => vec![RunEvent::ToolUse {
                    name: format!("{}/{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("?")),
                    summary: truncate(&item["arguments"].to_string(), 160),
                }],
                Some("web_search") => {
                    vec![RunEvent::ToolUse { name: "web_search".into(), summary: truncate(item["query"].as_str().unwrap_or(""), 160) }]
                }
                _ => vec![],
            },
            Some("item.completed") => match item["type"].as_str() {
                Some("agent_message") => {
                    let t = item["text"].as_str().unwrap_or("").to_string();
                    *self.last_message.lock().unwrap() = t.clone();
                    if t.trim().is_empty() {
                        vec![]
                    } else {
                        vec![RunEvent::Text(t)]
                    }
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
                        &item["changes"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|c| c["path"].as_str()).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default(),
                        160,
                    ),
                }],
                Some("error") => vec![RunEvent::Stderr(item["message"].as_str().unwrap_or("").to_string())],
                _ => vec![],
            },
            Some("turn.completed") => {
                let u = &v["usage"];
                vec![
                    RunEvent::Usage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cost_usd: None,
                    },
                    RunEvent::Done { ok: true, summary: self.last_message.lock().unwrap().clone() },
                ]
            }
            Some("turn.failed") => {
                vec![RunEvent::Done { ok: false, summary: v["error"]["message"].as_str().unwrap_or("turn failed").to_string() }]
            }
            Some("error") => vec![RunEvent::Stderr(v["message"].as_str().unwrap_or("").to_string())],
            _ => vec![],
        }
    }

    fn preflight(&self) -> Result<String> {
        version("codex")
    }

    /// Codex exposes no usage API; every session rollout logs `rate_limits` on each token_count event.
    fn usage(&self) -> Result<Usage> {
        let sessions = dirs::home_dir().context("no home")?.join(".codex/sessions");
        let mut rollouts: Vec<_> = walkdir::WalkDir::new(&sessions)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.into_path())))
            .collect();
        rollouts.sort_by(|a, b| b.0.cmp(&a.0));
        // A just-started session has no token_count yet; fall through to the next newest.
        let rl = rollouts
            .iter()
            .take(20)
            .filter_map(|(_, p)| std::fs::read_to_string(p).ok())
            .find_map(|text| {
                text.lines()
                    .rev()
                    .filter(|l| l.contains("rate_limits"))
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .map(|v| v["payload"]["rate_limits"].clone())
                    .find(|r| !r.is_null())
            })
            .context("no rate_limits in recent codex sessions")?;
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
        Ok(Usage { windows }.expired_reset())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_maps_to_events() {
        let c = Codex::default();
        assert!(matches!(c.parse_line(r#"{"type":"thread.started","thread_id":"t1"}"#)[0], RunEvent::Init { .. }));
        let ev = c.parse_line(r#"{"type":"item.started","item":{"type":"command_execution","command":"date"}}"#);
        assert!(matches!(&ev[0], RunEvent::ToolUse { name, summary } if name == "shell" && summary == "date"));
        let ev = c.parse_line(r#"{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"x","exit_code":1}}"#);
        assert!(matches!(ev[0], RunEvent::ToolResult { is_error: true, .. }));
        c.parse_line(r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#);
        let ev = c.parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2}}"#);
        assert!(matches!(ev[0], RunEvent::Usage { input: 5, output: 2, .. }));
        assert!(matches!(&ev[1], RunEvent::Done { ok: true, summary } if summary == "done"));
        let ev = c.parse_line(r#"{"type":"turn.failed","error":{"message":"boom"}}"#);
        assert!(matches!(&ev[0], RunEvent::Done { ok: false, summary } if summary == "boom"));
    }
}
