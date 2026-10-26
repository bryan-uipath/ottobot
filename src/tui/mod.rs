//! Terminal UI: board rail (now / attention / quota / next) or run history, beside the session view.

mod ui;

use crate::client::Backend;
use crate::proto::Snapshot;
use crate::run::{RunEvent, RunStatus};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::collections::HashMap;
use std::time::Duration;

/// Focus target. Now/Attention/Next are board sections; in history mode any of them means "rail".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Now,
    Attention,
    Next,
    Session,
}

pub struct Ui {
    pub pane: Pane,
    /// Board section whose selection drives the session pane; never Session.
    pub rail: Pane,
    pub now_idx: usize,
    pub att_idx: usize,
    pub next_idx: usize,
    /// Selection in history mode, over `visible_runs()`.
    pub run_idx: usize,
    /// `h`: rail shows the full run list instead of the board.
    pub history: bool,
    /// `?`: about overlay.
    pub about: bool,
    /// Initial selection made (waits for the first non-empty snapshot).
    pub picked: bool,
    /// Skill of the run last shown in the session pane; lets a finished NOW run stay selected.
    pub shown_skill: Option<String>,
    pub session_scroll: u16,
    /// Auto-follow session output while the run is live.
    pub follow: bool,
    pub show_log: bool,
    /// Only show runs for this skill in history. Enter on a NEXT skill toggles it; Esc clears.
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
    /// Error from `r`/`R`, kept for a few seconds so a successful refresh doesn't erase it.
    pub action_error: Option<(std::time::Instant, String)>,
}

pub async fn run(mut backend: Backend) -> Result<()> {
    let mut term = ratatui::init();
    let mut ui = Ui {
        pane: Pane::Now,
        rail: Pane::Now,
        now_idx: 0,
        att_idx: 0,
        next_idx: 0,
        run_idx: 0,
        history: false,
        about: false,
        picked: false,
        shown_skill: None,
        session_scroll: 0,
        follow: true,
        show_log: false,
        run_filter: None,
        frame: 0,
    };
    let mut m = Model::default();
    let res = loop {
        ui.frame = ui.frame.wrapping_add(1);
        refresh(&mut backend, &mut m, &mut ui).await;
        if m.action_error.as_ref().is_some_and(|(t, _)| t.elapsed() > Duration::from_secs(5)) {
            m.action_error = None;
        }
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

/// Pull a fresh snapshot, then new events of running runs (NOW tool line) and the selected run.
async fn refresh(backend: &mut Backend, m: &mut Model, ui: &mut Ui) {
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
    if ui.picked {
        settle(&m.snap, ui);
    } else {
        pick_initial(&m.snap, ui);
    }
    ui.shown_skill = ui::selected_run(&m.snap, ui).map(|r| r.skill.clone());
    let mut ids: Vec<String> = m.snap.runs.iter().filter(|r| r.status == RunStatus::Running).map(|r| r.id.clone()).collect();
    if let Some(r) = ui::selected_run(&m.snap, ui) {
        if !ids.contains(&r.id) {
            ids.push(r.id.clone());
        }
    }
    for id in ids {
        let cache = m.events.entry(id.clone()).or_default();
        match backend.events(&id, cache.len()).await {
            Ok(new) => cache.extend(new),
            Err(e) => m.error = Some(format!("{e:#}")),
        }
    }
}

/// First running run, else newest failure, else first NEXT row. Waits until skills have loaded.
fn pick_initial(st: &Snapshot, ui: &mut Ui) {
    if st.skills.is_empty() && st.skill_errors.is_empty() {
        return;
    }
    let b = ui::board(st);
    let first_fail = b.attn.iter().position(|a| matches!(a, ui::Attn::Run(_)));
    (ui.rail, ui.now_idx, ui.att_idx, ui.next_idx) = match (b.now.is_empty(), first_fail) {
        (false, _) => (Pane::Now, 0, 0, 0),
        (true, Some(i)) => (Pane::Attention, 0, i, 0),
        (true, None) => (Pane::Next, 0, 0, 0),
    };
    ui.pane = ui.rail;
    ui.picked = true;
}

/// Keep board indices in range. A selected NOW run that finished moves to its skill's NEXT row.
fn settle(st: &Snapshot, ui: &mut Ui) {
    let b = ui::board(st);
    if ui.rail == Pane::Now && ui.now_idx >= b.now.len() {
        match ui.shown_skill.as_ref().and_then(|s| b.next.iter().position(|v| v.name == *s)) {
            Some(i) => {
                ui.next_idx = i;
                ui.rail = Pane::Next;
                if ui.pane == Pane::Now {
                    ui.pane = Pane::Next;
                }
            }
            None => ui.now_idx = b.now.len().saturating_sub(1),
        }
    }
    ui.att_idx = ui.att_idx.min(b.attn.len().saturating_sub(1));
    ui.next_idx = ui.next_idx.min(b.next.len().saturating_sub(1));
}

/// Board sections that have selectable rows, in tab order.
fn sections(st: &Snapshot) -> Vec<Pane> {
    let b = ui::board(st);
    [(Pane::Now, b.now.len()), (Pane::Attention, b.attn.len()), (Pane::Next, b.next.len())].into_iter().filter(|(_, n)| *n > 0).map(|(p, _)| p).collect()
}

/// Tab order: board sections (non-empty) then session; history mode is just rail ↔ session.
fn cycle(ui: &mut Ui, st: &Snapshot, back: bool) {
    let mut order = if ui.history { vec![ui.rail] } else { sections(st) };
    order.push(Pane::Session);
    let cur = order.iter().position(|p| *p == ui.pane).unwrap_or(0);
    let n = order.len();
    ui.pane = order[if back { (cur + n - 1) % n } else { (cur + 1) % n }];
    if ui.pane != Pane::Session {
        ui.rail = ui.pane;
        reset_session(ui);
    }
}

fn reset_session(ui: &mut Ui) {
    ui.session_scroll = 0;
    ui.follow = true;
}

/// Move the focused rail selection by `d`, clamped to the rows shown.
fn move_sel(ui: &mut Ui, st: &Snapshot, d: isize) {
    let b = ui::board(st);
    let n = match (ui.history, ui.rail) {
        (true, _) => ui::visible_runs(st, ui).len(),
        (false, Pane::Now) => b.now.len(),
        (false, Pane::Attention) => b.attn.len(),
        (false, _) => b.next.len(),
    };
    let idx = match (ui.history, ui.rail) {
        (true, _) => &mut ui.run_idx,
        (false, Pane::Now) => &mut ui.now_idx,
        (false, Pane::Attention) => &mut ui.att_idx,
        (false, _) => &mut ui.next_idx,
    };
    *idx = idx.saturating_add_signed(d).min(n.saturating_sub(1));
    reset_session(ui);
}

async fn handle_key(code: KeyCode, ui: &mut Ui, m: &mut Model, backend: &mut Backend) {
    if ui.about {
        if matches!(code, KeyCode::Esc | KeyCode::Char('?')) {
            ui.about = false;
        }
        return;
    }
    match code {
        KeyCode::Tab => cycle(ui, &m.snap, false),
        KeyCode::BackTab => cycle(ui, &m.snap, true),
        KeyCode::Char('j') | KeyCode::Down if ui.pane == Pane::Session => {
            ui.follow = false;
            ui.session_scroll = ui.session_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up if ui.pane == Pane::Session => {
            ui.follow = false;
            ui.session_scroll = ui.session_scroll.saturating_sub(1);
        }
        KeyCode::Char('j') | KeyCode::Down => move_sel(ui, &m.snap, 1),
        KeyCode::Char('k') | KeyCode::Up => move_sel(ui, &m.snap, -1),
        KeyCode::Char('G') => ui.follow = true,
        KeyCode::Char('g') => {
            ui.follow = false;
            ui.session_scroll = 0;
        }
        KeyCode::Enter => match (ui.history, ui.pane) {
            (_, Pane::Session) => {}
            // NEXT skill: toggle the history filter and show history.
            (false, Pane::Next) => {
                let name = ui::board(&m.snap).next.get(ui.next_idx).map(|v| v.name.clone());
                ui.run_filter = if ui.run_filter == name { None } else { name };
                ui.history = true;
                ui.run_idx = 0;
                reset_session(ui);
            }
            _ => ui.pane = Pane::Session,
        },
        KeyCode::Esc => {
            ui.run_filter = None;
            ui.run_idx = 0;
        }
        KeyCode::Char('h') => {
            ui.history = !ui.history;
            if ui.pane != Pane::Session && !ui.history {
                ui.pane = ui.rail;
            }
            reset_session(ui);
        }
        KeyCode::Char('?') => ui.about = true,
        KeyCode::Char('r') => {
            let Some(name) = ui::selected_skill(&m.snap, ui) else { return };
            if let Err(e) = backend.run_now(&name).await {
                m.action_error = Some((std::time::Instant::now(), format!("{e:#}")));
                return;
            }
            // Follow the new run: history → top row; board → this skill's NEXT row (its latest run).
            if ui.history {
                ui.run_idx = 0;
            } else if let Some(i) = ui::board(&m.snap).next.iter().position(|v| v.name == name) {
                ui.next_idx = i;
                ui.rail = Pane::Next;
                if ui.pane != Pane::Session {
                    ui.pane = Pane::Next;
                }
            }
            reset_session(ui);
        }
        KeyCode::Char('R') => {
            if let Err(e) = backend.reload().await {
                m.action_error = Some((std::time::Instant::now(), format!("{e:#}")));
            }
        }
        KeyCode::Char('l') => ui.show_log = !ui.show_log,
        _ => {}
    }
}
