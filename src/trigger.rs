//! Trigger evaluation. Cron is computed locally; event triggers are checked by scripts/agents (slice 2).

use crate::skill::TriggerSpec;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use croner::Cron;
use std::time::Duration;

pub const DEFAULT_EVENT_CHECK: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_EVENT_COOLDOWN: Duration = Duration::from_secs(8 * 3600);

#[derive(Debug, Clone)]
pub enum Trigger {
    Cron {
        pattern: String,
        cron: Box<Cron>,
    },
    /// `watch`: the checker is always an agent with memory, never a compiled script.
    Event {
        description: String,
        check_every: Duration,
        watch: bool,
    },
}

impl Trigger {
    pub fn from_spec(spec: &TriggerSpec) -> Result<Self> {
        if let Some(p) = &spec.cron {
            let cron = Cron::new(p).with_seconds_optional().parse().with_context(|| format!("cron `{p}`"))?;
            return Ok(Trigger::Cron { pattern: p.clone(), cron: Box::new(cron) });
        }
        let watch = spec.watch.is_some();
        let description = spec.event.clone().or_else(|| spec.watch.clone()).context("no trigger")?;
        Ok(Trigger::Event { description, check_every: spec.check_every.unwrap_or(DEFAULT_EVENT_CHECK), watch })
    }

    /// Cron default is 0 (the schedule is the dedupe). Event default is 8h.
    pub fn default_cooldown(&self) -> Duration {
        match self {
            Trigger::Cron { .. } => Duration::ZERO,
            Trigger::Event { .. } => DEFAULT_EVENT_COOLDOWN,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Trigger::Cron { pattern, .. } => format!("cron {pattern}"),
            Trigger::Event { description, check_every, watch } => {
                format!("{} every {} · {}", if *watch { "watch" } else { "event" }, humantime::format_duration(*check_every), description)
            }
        }
    }

    /// Next fire strictly after `after`.
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match self {
            Trigger::Cron { cron, .. } => cron.find_next_occurrence(&after, false).ok(),
            Trigger::Event { .. } => None,
        }
    }
}
