//! Model routing: a skill may list several `runner/model` candidates; the one whose runner has
//! the most quota left wins. Aliases from config (`models: {tier1: [...]}`) expand first.

use crate::runner::{self, RunnerUsage};

const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
/// Health of each runner (`preflight()` result) so unavailable runners are never chosen.
pub type RunnerHealth = [(String, Result<String, String>)];
use crate::skill::ModelSpec;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub runner: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Why this one, for the scheduler log. Empty when there was no choice to make.
    pub why: String,
}

type Parsed = (String, Option<String>, Option<String>);

/// `runner/model@effort`: runner and effort optional. Bare `runner` → (runner, None); bare `model` → default runner.
fn parse(cand: &str, default_runner: &str) -> Parsed {
    let known = |s: &str| runner::by_name(s).is_ok();
    // Only a known effort level is a suffix; `@` inside a model id (dated ids) stays in the model.
    let (cand, effort) = match cand.rsplit_once('@') {
        Some((c, e)) if EFFORTS.contains(&e) => (c, Some(e.to_string())),
        _ => (cand, None),
    };
    let (runner, model) = match cand.split_once('/') {
        Some((r, m)) if known(r) => (r.to_string(), Some(m.to_string())),
        None if known(cand) => (cand.to_string(), None),
        _ => (default_runner.to_string(), Some(cand.to_string())),
    };
    (runner, model, effort)
}

/// Quota left for a runner: the tightest window. Unknown usage scores -1 so a known runner always
/// beats an unknown one; a runner that failed preflight scores -2 so it is only ever the last resort.
fn left(usage: &RunnerUsage, health: &RunnerHealth, runner: &str) -> f64 {
    if health.iter().any(|(n, h)| n == runner && h.is_err()) {
        return -2.0;
    }
    match usage.iter().find(|(n, _)| n == runner) {
        Some((_, Ok(ws))) if !ws.is_empty() => ws.iter().map(|w| w.left_percent()).fold(100.0, f64::min),
        _ => -1.0,
    }
}

/// Expand aliases up to 3 levels; empty aliases contribute nothing.
fn expand(cands: Vec<String>, aliases: &HashMap<String, Vec<String>>, depth: u8) -> Vec<String> {
    cands
        .into_iter()
        .flat_map(|c| match aliases.get(&c) {
            Some(v) if depth > 0 => expand(v.clone(), aliases, depth - 1),
            Some(_) => vec![],
            None => vec![c],
        })
        .collect()
}

pub fn resolve(spec: Option<&ModelSpec>, default_runner: &str, aliases: &HashMap<String, Vec<String>>, usage: &RunnerUsage, health: &RunnerHealth) -> Choice {
    let cands = expand(spec.map(ModelSpec::candidates).unwrap_or_default(), aliases, 3);
    if cands.is_empty() {
        return Choice { runner: default_runner.to_string(), model: None, effort: None, why: String::new() };
    }
    let parsed: Vec<Parsed> = cands.iter().map(|c| parse(c, default_runner)).collect();
    if parsed.len() == 1 {
        let (runner, model, effort) = parsed.into_iter().next().unwrap_or_default();
        return Choice { runner, model, effort, why: String::new() };
    }
    // Highest quota left wins; strict `>` keeps list order on ties.
    let scored: Vec<(f64, &Parsed)> = parsed.iter().map(|p| (left(usage, health, &p.0), p)).collect();
    let mut best = scored[0];
    for s in &scored[1..] {
        if s.0 > best.0 {
            best = *s;
        }
    }
    let (score, chosen) = best;
    let (runner, model, effort) = chosen.clone();
    let others: Vec<String> = scored.iter().filter(|(_, p)| *p != chosen).map(|(s, p)| format!("{} {s:.0}%", p.0)).collect();
    Choice {
        why: format!("routed to {}{} ({score:.0}% left vs {})", runner, model.as_ref().map(|m| format!("/{m}")).unwrap_or_default(), others.join(", ")),
        runner,
        model,
        effort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::UsageWindow;

    fn usage(claude_left: f64, codex_left: f64) -> RunnerUsage {
        let w = |l: f64| Ok(vec![UsageWindow { label: "7d".into(), used_percent: 100.0 - l, resets_at: None }]);
        vec![("claude".into(), w(claude_left)), ("codex".into(), w(codex_left))]
    }

    #[test]
    fn picks_most_headroom_and_expands_aliases() {
        let ok: Vec<(String, Result<String, String>)> = vec![("claude".into(), Ok("1".into())), ("codex".into(), Ok("1".into()))];
        let none = HashMap::new();
        let spec = ModelSpec::Many(vec!["codex/gpt-6-astra".into(), "claude/opus".into()]);
        let c = resolve(Some(&spec), "claude", &none, &usage(20.0, 90.0), &ok);
        assert_eq!((c.runner.as_str(), c.model.as_deref()), ("codex", Some("gpt-6-astra")));
        let c = resolve(Some(&spec), "claude", &none, &usage(95.0, 90.0), &ok);
        assert_eq!((c.runner.as_str(), c.model.as_deref()), ("claude", Some("opus")));
        // Nested alias, tie keeps order.
        let aliases = HashMap::from([("tier1".to_string(), vec!["best".to_string()]), ("best".to_string(), vec!["codex/gpt-6-astra".to_string(), "claude/opus".to_string()])]);
        let c = resolve(Some(&ModelSpec::One("tier1".into())), "claude", &aliases, &usage(50.0, 50.0), &ok);
        assert_eq!(c.runner, "codex");
        let c = resolve(Some(&ModelSpec::One("haiku".into())), "claude", &none, &usage(0.0, 100.0), &ok);
        assert_eq!((c.runner.as_str(), c.model.as_deref(), c.why.as_str()), ("claude", Some("haiku"), ""));
        let c = resolve(Some(&ModelSpec::One("codex/gpt-6-astra@medium".into())), "claude", &none, &usage(0.0, 0.0), &ok);
        assert_eq!((c.runner.as_str(), c.model.as_deref(), c.effort.as_deref()), ("codex", Some("gpt-6-astra"), Some("medium")));
        // `@` that is not an effort stays in the model id.
        let c = resolve(Some(&ModelSpec::One("claude/claude-sonnet-4@20250514".into())), "claude", &none, &usage(0.0, 0.0), &ok);
        assert_eq!((c.model.as_deref(), c.effort.as_deref()), (Some("claude-sonnet-4@20250514"), None));
        // Unknown usage loses to known; unhealthy runner loses to everything.
        let c = resolve(Some(&spec), "claude", &none, &vec![("claude".into(), Ok(vec![UsageWindow { label: "7d".into(), used_percent: 70.0, resets_at: None }]))], &ok);
        assert_eq!(c.runner, "claude");
        let sick: Vec<(String, Result<String, String>)> = vec![("codex".into(), Err("not on PATH".into()))];
        let c = resolve(Some(&spec), "claude", &none, &usage(1.0, 99.0), &sick);
        assert_eq!(c.runner, "claude");
    }
}
