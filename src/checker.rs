//! Event trigger checks. Prose condition → checker script (compiled once per
//! trigger hash, cached on disk). Script errors fall back to an agent evaluation;
//! repeated errors delete the script so it recompiles.

use crate::run::RunEvent;
use crate::runner::{self, Invocation};
use crate::skill::Skill;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2 * 60);
pub const AGENT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Script failures before it is deleted and recompiled.
pub const MAX_SCRIPT_FAILURES: u32 = 3;

/// What a checker (script or agent) reports. `key` dedupes occurrences.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct CheckResult {
    pub fired: bool,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Script,
    Agent,
}

pub struct Outcome {
    pub result: CheckResult,
    pub mode: Mode,
    /// The script ran and errored (agent answered instead). Caller counts these.
    pub script_failed: Option<String>,
}

pub struct Checker {
    pub skill: Skill,
    pub description: String,
    /// Where compiled scripts live: `<store>/checkers/<skill>-<trigger_hash>`.
    pub dir: PathBuf,
    pub cwd: PathBuf,
    pub runner: String,
}

impl Checker {
    pub fn script_path(&self) -> PathBuf {
        self.dir.join(format!("{}-{}", self.skill.name, self.skill.trigger_hash))
    }

    /// Compile if needed, run the script, fall back to an agent on any error.
    pub async fn check(&self, failures: u32) -> Result<Outcome> {
        let path = self.script_path();
        if !path.exists() {
            match self.compile().await {
                Ok(()) => tracing::info!("{}: compiled checker {}", self.skill.name, path.display()),
                Err(e) => {
                    tracing::warn!("{}: compile failed, using agent: {e:#}", self.skill.name);
                    return Ok(Outcome { result: self.evaluate_with_agent().await?, mode: Mode::Agent, script_failed: None });
                }
            }
        }
        match self.run_script().await {
            Ok(result) => Ok(Outcome { result, mode: Mode::Script, script_failed: None }),
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("{}: checker script failed ({msg}), using agent", self.skill.name);
                if failures + 1 >= MAX_SCRIPT_FAILURES {
                    let _ = std::fs::remove_file(&path);
                    tracing::info!("{}: removed checker after {} failures", self.skill.name, failures + 1);
                }
                Ok(Outcome { result: self.evaluate_with_agent().await?, mode: Mode::Agent, script_failed: Some(msg) })
            }
        }
    }

    /// Ask the checker model to write the script to `script_path()` and verify it once.
    async fn compile(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.script_path();
        let prompt = format!(
            "Write a checker script for this event condition:\n\n  {desc}\n\n\
             Save it to exactly `{path}` (no other files). Pick bash, node, or python, \
             whichever fits; start with a shebang and chmod +x it.\n\n\
             Contract: no arguments, finishes in under 60s, prints exactly one JSON line to stdout:\n\
             {{\"fired\": bool, \"key\": string, \"summary\": string}}\n\
             - fired: the condition holds right now\n\
             - key: stable id of this occurrence (run id, PR number, date...) so the same \
               occurrence is never reported twice; \"\" when not fired\n\
             - summary: one human line, include URLs/ids the downstream agent needs\n\
             Exit non-zero only when the check itself cannot run (auth, network). \
             Use CLIs on this machine (gh, curl, jq, az...). Run it once to verify the output \
             parses. Do not report anything except the file.\n\n\
             Skill body, for context on what the condition means:\n\n{body}",
            desc = self.description,
            path = path.display(),
            body = self.skill.body,
        );
        let inv = Invocation { prompt, cwd: self.dir.clone(), model: Some(self.checker_model()), permission_mode: self.skill.front.permission_mode.clone() };
        let (ok, summary) = run_to_end(&self.runner, inv, AGENT_TIMEOUT).await?;
        if !ok {
            bail!("compile agent failed: {summary}");
        }
        if !path.exists() {
            bail!("compile agent did not write {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    async fn run_script(&self) -> Result<CheckResult> {
        let path = self.script_path();
        let out = tokio::time::timeout(
            CHECK_TIMEOUT,
            tokio::process::Command::new(&path)
                .current_dir(&self.cwd)
                .env("OTTO_SKILL", &self.skill.name)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("checker timed out")??;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("exit {}: {}", out.status.code().unwrap_or(-1), tail(&err, 300));
        }
        parse_result(&stdout).with_context(|| format!("bad checker output: {}", tail(&stdout, 300)))
    }

    /// Fallback: the checker model investigates with tools and answers in JSON.
    async fn evaluate_with_agent(&self) -> Result<CheckResult> {
        let prompt = format!(
            "Determine whether this condition holds right now:\n\n  {desc}\n\n\
             Investigate with the tools available (gh, curl, jq...). Do not fix anything.\n\
             Your final message must be exactly one JSON line, nothing else:\n\
             {{\"fired\": bool, \"key\": string, \"summary\": string}}\n\
             key = stable id of this occurrence (\"\" when not fired); summary = one human line \
             with the URLs/ids a downstream agent needs.\n\n\
             Skill body, for context:\n\n{body}",
            desc = self.description,
            body = self.skill.body,
        );
        let inv = Invocation { prompt, cwd: self.cwd.clone(), model: Some(self.checker_model()), permission_mode: self.skill.front.permission_mode.clone() };
        let (ok, summary) = run_to_end(&self.runner, inv, AGENT_TIMEOUT).await?;
        if !ok {
            bail!("agent check failed: {}", tail(&summary, 300));
        }
        parse_result(&summary).with_context(|| format!("agent gave no JSON verdict: {}", tail(&summary, 300)))
    }

    fn checker_model(&self) -> String {
        self.skill.front.checker_model.clone().unwrap_or_else(|| "sonnet".into())
    }
}

/// Accepts a whole-output object (pretty-printed or fenced), else the last object
/// found by scanning `{` positions from the end. Tolerates chatter around it.
fn parse_result(text: &str) -> Option<CheckResult> {
    let t = text.trim().trim_matches('`').trim_start_matches("json").trim();
    if let Ok(r) = serde_json::from_str(t) {
        return Some(r);
    }
    t.rmatch_indices('{').find_map(|(i, _)| serde_json::Deserializer::from_str(&t[i..]).into_iter::<CheckResult>().next()?.ok())
}

/// Run a runner invocation to completion. Returns the Done flag and summary.
async fn run_to_end(runner: &str, inv: Invocation, timeout: Duration) -> Result<(bool, String)> {
    let runner = runner::by_name(runner)?;
    let mut cmd = runner.command(&inv);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().context("piped")?;
    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut done = (false, String::new());
        while let Ok(Some(l)) = lines.next_line().await {
            for e in runner.parse_line(&l) {
                if let RunEvent::Done { ok, summary } = e {
                    done = (ok, summary);
                }
            }
        }
        done
    };
    let done = match tokio::time::timeout(timeout, read).await {
        Ok(d) => d,
        Err(_) => {
            let _ = child.kill().await;
            bail!("timed out after {}", humantime::format_duration(timeout));
        }
    };
    let status = child.wait().await?;
    Ok((done.0 && status.success(), done.1))
}

fn tail(s: &str, n: usize) -> String {
    let s = s.trim();
    let count = s.chars().count();
    if count <= n { s.to_string() } else { format!("…{}", s.chars().skip(count - n).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::parse_result;

    #[test]
    fn parses_compact_pretty_fenced_and_trailing() {
        assert!(parse_result(r#"{"fired":true,"key":"1","summary":"s"}"#).unwrap().fired);
        assert_eq!(parse_result("{\n  \"fired\": false,\n  \"key\": \"\",\n  \"summary\": \"x\"\n}").unwrap().summary, "x");
        assert_eq!(parse_result("```json\n{\"fired\":true,\"key\":\"a\"}\n```").unwrap().key, "a");
        assert_eq!(parse_result("checked run {12}\n{\"fired\":true,\"key\":\"b\"} done").unwrap().key, "b");
        assert!(parse_result("nothing here").is_none());
    }
}
