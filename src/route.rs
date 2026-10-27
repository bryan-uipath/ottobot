//! Model routing: a skill may list several `runner/model` candidates; the one whose runner has
//! the most quota left wins. Aliases from config (`models: {tier1: [...]}`) expand first.

use crate::runner::{self, RunnerUsage};
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
    let (cand, effort) = match cand.rsplit_once('@') {
        Some((c, e)) => (c, Some(e.to_string())),
        None => (cand, None),
    };
    let (runner, model) = match cand.split_once('/') {
        Some((r, m)) if known(r) => (r.to_string(), Some(m.to_string())),
        None if known(cand) => (cand.to_string(), None),
        _ => (default_runner.to_string(), Some(cand.to_string())),
    };
    (runner, model, effort)
}

/// Quota left for a runner: the tightest window. Unknown usage scores as 50 so it neither wins nor loses by default.
fn left(usage: &RunnerUsage, runner: &str) -> f64 {
    match usage.iter().find(|(n, _)| n == runner) {
        Some((_, Ok(ws))) if !ws.is_empty() => ws.iter().map(|w| w.left_percent()).fold(100.0, f64::min),
        _ => 50.0,
    }
}

pub fn resolve(spec: Option<&ModelSpec>, default_runner: &str, aliases: &HashMap<String, Vec<String>>, usage: &RunnerUsage) -> Choice {
    let mut cands: Vec<String> = Vec::new();
    for c in spec.map(ModelSpec::candidates).unwrap_or_default() {
        match aliases.get(&c) {
            Some(v) => cands.extend(v.iter().cloned()),
            None => cands.push(c),
        }
    }
    if cands.is_empty() {
        return Choice { runner: default_runner.to_string(), model: None, effort: None, why: String::new() };
    }
    let parsed: Vec<Parsed> = cands.iter().map(|c| parse(c, default_runner)).collect();
    if parsed.len() == 1 {
        let (runner, model, effort) = parsed.into_iter().next().unwrap_or_default();
        return Choice { runner, model, effort, why: String::new() };
    }
    // Highest quota left wins; ties keep list order.
    let scored: Vec<(f64, &Parsed)> = parsed.iter().map(|p| (left(usage, &p.0), p)).collect();
    let best = scored.iter().copied().fold(None::<(f64, &Parsed)>, |acc, s| match acc {
        Some(a) if a.0 >= s.0 => Some(a),
        _ => Some(s),
    });
    let (score, chosen) = best.unwrap_or((0.0, &parsed[0]));
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
        let spec = ModelSpec::Many(vec!["codex/gpt-6-astra".into(), "claude/opus".into()]);
        let c = resolve(Some(&spec), "claude", &HashMap::new(), &usage(20.0, 90.0));
        assert_eq!((c.runner.as_str(), c.model.as_deref()), ("codex", Some("gpt-6-astra")));
        let c = resolve(Some(&spec), "claude", &HashMap::new(), &usage(95.0, 90.0));
        assert_eq!((c.runner.as_str(), c.model.as_deref()), ("claude", Some("opus")));
        let aliases = HashMap::from([("tier1".to_string(), vec!["codex/gpt-6-astra".to_string(), "claude/opus".to_string()])]);
        let c = resolve(Some(&ModelSpec::One("tier1".into())), "claude", &aliases, &usage(50.0, 50.0));
        assert_eq!(c.runner, "codex"); // tie keeps order
        let c = resolve(Some(&ModelSpec::One("haiku".into())), "claude", &HashMap::new(), &usage(0.0, 100.0));
        assert_eq!((c.runner.as_str(), c.model.as_deref(), c.why.as_str()), ("claude", Some("haiku"), ""));
        let c = resolve(Some(&ModelSpec::One("codex/gpt-6-astra@medium".into())), "claude", &HashMap::new(), &usage(0.0, 0.0));
        assert_eq!((c.runner.as_str(), c.model.as_deref(), c.effort.as_deref()), ("codex", Some("gpt-6-astra"), Some("medium")));
    }
}
