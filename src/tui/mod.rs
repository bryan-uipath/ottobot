//! Terminal UI: skills / runs / session panes with an Otto header.

mod ui;

use crate::client::Backend;
use crate::proto::Snapshot;
use crate::run::RunEvent;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Skills,
    Runs,
    Session,
}

pub struct Ui {
    pub pane: Pane,
    pub skill_idx: usize,
    pub run_idx: usize,
    pub session_scroll: u16,
    /// Auto-follow session output while the run is live.
    pub follow: bool,
    pub show_log: bool,
    /// Only show runs for this skill. Enter on a skill toggles it; Esc clears.
    pub run_filter: Option<String>,
    /// Incremented every draw; drives spinners.
    pub frame: usize,
}

/// What the TUI draws: the latest snapshot plus events fetched so far, per run id.
#[derive(Default)]
pub struct Model {
    pub snap: Snapshot,
    pub events: HashMap<String, Vec<RunEvent>>,
    /// Backend error from the last refresh ("daemon disconnected: …"). Last snapshot stays on screen.
    pub error: Option<String>,
}

pub async fn run(mut backend: Backend) -> Result<()> {
    let mut term = ratatui::init();
    let mut ui = Ui { pane: Pane::Skills, skill_idx: 0, run_idx: 0, session_scroll: 0, follow: true, show_log: false, run_filter: None, frame: 0 };
    let mut m = Model::default();
    let res = loop {
        ui.frame = ui.frame.wrapping_add(1);
        refresh(&mut backend, &mut m, &ui).await;
        if let Err(e) = term.draw(|f| ui::draw(f, &m, &mut ui)) {
            break Err(e.into());
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl_c || k.code == KeyCode::Char('q') {
                    break Ok(());
                }
                handle_key(k.code, &mut ui, &mut m, &mut backend).await;
            }
        }
    };
    ratatui::restore();
    res
}

/// Pull a fresh snapshot, then only the new events of the selected run.
async fn refresh(backend: &mut Backend, m: &mut Model, ui: &Ui) {
    match backend.snapshot().await {
        Ok(s) => {
            m.snap = s;
            m.error = None;
        }
        Err(e) => {
            m.error = Some(format!("{e:#}"));
            return;
        }
    }
    let Some(id) = ui::visible_runs(&m.snap, ui).get(ui.run_idx).map(|r| r.id.clone()) else { return };
    let cache = m.events.entry(id.clone()).or_default();
    match backend.events(&id, cache.len()).await {
        Ok(new) => cache.extend(new),
        Err(e) => m.error = Some(format!("{e:#}")),
    }
}

async fn handle_key(code: KeyCode, ui: &mut Ui, m: &mut Model, backend: &mut Backend) {
    let (n_skills, n_runs) = (m.snap.skills.len(), ui::visible_runs(&m.snap, ui).len());
    match code {
        KeyCode::Tab => {
            ui.pane = match ui.pane {
                Pane::Skills => Pane::Runs,
                Pane::Runs => Pane::Session,
                Pane::Session => Pane::Skills,
            }
        }
        KeyCode::BackTab => {
            ui.pane = match ui.pane {
                Pane::Skills => Pane::Session,
                Pane::Runs => Pane::Skills,
                Pane::Session => Pane::Runs,
            }
        }
        KeyCode::Char('j') | KeyCode::Down => match ui.pane {
            Pane::Skills => ui.skill_idx = (ui.skill_idx + 1).min(n_skills.saturating_sub(1)),
            Pane::Runs => {
                ui.run_idx = (ui.run_idx + 1).min(n_runs.saturating_sub(1));
                ui.session_scroll = 0;
                ui.follow = true;
            }
            Pane::Session => {
                ui.follow = false;
                ui.session_scroll = ui.session_scroll.saturating_add(1);
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match ui.pane {
            Pane::Skills => ui.skill_idx = ui.skill_idx.saturating_sub(1),
            Pane::Runs => {
                ui.run_idx = ui.run_idx.saturating_sub(1);
                ui.session_scroll = 0;
                ui.follow = true;
            }
            Pane::Session => {
                ui.follow = false;
                ui.session_scroll = ui.session_scroll.saturating_sub(1);
            }
        },
        KeyCode::Char('G') => ui.follow = true,
        KeyCode::Char('g') => {
            ui.follow = false;
            ui.session_scroll = 0;
        }
        KeyCode::Enter => match ui.pane {
            Pane::Runs => ui.pane = Pane::Session,
            Pane::Skills => {
                let name = m.snap.skills.get(ui.skill_idx).map(|v| v.name.clone());
                ui.run_filter = if ui.run_filter == name { None } else { name };
                ui.run_idx = 0;
                ui.follow = true;
            }
            Pane::Session => {}
        },
        KeyCode::Esc => {
            ui.run_filter = None;
            ui.run_idx = 0;
        }
        KeyCode::Char('r') => {
            let name = m.snap.skills.get(ui.skill_idx).map(|v| v.name.clone());
            if let Some(name) = name {
                if let Err(e) = backend.run_now(&name).await {
                    m.error = Some(format!("{e:#}"));
                }
                ui.run_idx = 0;
                ui.follow = true;
            }
        }
        KeyCode::Char('R') => {
            if let Err(e) = backend.reload().await {
                m.error = Some(format!("{e:#}"));
            }
        }
        KeyCode::Char('l') => ui.show_log = !ui.show_log,
        _ => {}
    }
}
