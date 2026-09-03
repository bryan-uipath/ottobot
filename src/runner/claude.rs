//! `claude -p --output-format stream-json` runner.

use super::{which, Invocation, Runner};
use crate::run::RunEvent;
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

pub struct Claude;

impl Runner for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn command(&self, inv: &Invocation) -> Command {
        let mut c = Command::new("claude");
        c.current_dir(&inv.cwd)
            .arg("-p")
            .arg(&inv.prompt)
            .args(["--output-format", "stream-json", "--verbose"])
            .args(["--permission-mode", inv.permission_mode.as_deref().unwrap_or("bypassPermissions")]);
        if let Some(m) = &inv.model {
            c.args(["--model", m]);
        }
        c
    }

    fn parse_line(&self, line: &str) -> Vec<RunEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return if line.trim().is_empty() { vec![] } else { vec![RunEvent::Raw(line.to_string())] };
        };
        match v["type"].as_str() {
            Some("system") if v["subtype"].as_str() == Some("init") => vec![RunEvent::Init {
                session: v["session_id"].as_str().unwrap_or("").to_string(),
                model: v["model"].as_str().map(String::from),
            }],
            Some("assistant") | Some("user") => content_events(&v["message"]["content"]),
            Some("result") => {
                let usage = &v["usage"];
                let mut out = vec![RunEvent::Usage {
                    input: usage["input_tokens"].as_u64().unwrap_or(0),
                    output: usage["output_tokens"].as_u64().unwrap_or(0),
                    cost_usd: v["total_cost_usd"].as_f64(),
                }];
                let ok = !v["is_error"].as_bool().unwrap_or(false) && v["subtype"].as_str() == Some("success");
                out.push(RunEvent::Done { ok, summary: v["result"].as_str().unwrap_or("").to_string() });
                out
            }
            _ => vec![],
        }
    }

    fn preflight(&self) -> Result<String> {
        which("claude").context("`claude` not on PATH")?;
        let out = std::process::Command::new("claude").arg("--version").output().context("claude --version")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

fn content_events(content: &Value) -> Vec<RunEvent> {
    let Some(items) = content.as_array() else { return vec![] };
    items
        .iter()
        .filter_map(|c| match c["type"].as_str() {
            Some("text") => c["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| RunEvent::Text(t.to_string())),
            Some("tool_use") => Some(RunEvent::ToolUse {
                name: c["name"].as_str().unwrap_or("?").to_string(),
                summary: tool_summary(&c["input"]),
            }),
            Some("tool_result") => Some(RunEvent::ToolResult {
                summary: truncate(&result_text(&c["content"]), 300),
                is_error: c["is_error"].as_bool().unwrap_or(false),
            }),
            _ => None,
        })
        .collect()
}

/// Pick the most descriptive input field for the one-line tool summary.
fn tool_summary(input: &Value) -> String {
    for k in ["command", "description", "file_path", "pattern", "query", "prompt", "url"] {
        if let Some(s) = input[k].as_str() {
            return truncate(s, 160);
        }
    }
    truncate(&input.to_string(), 160)
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim().replace('\n', " ⏎ ");
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}
