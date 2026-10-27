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
    /// Selection per board section by identity, so list churn/re-sorting doesn't move it.
    pub now_run: Option<String>,
    pub attn_sel: Option<ui::AttnKey>,
    pub next_skill: Option<String>,
    /// Last resolved row per section; fallback when the selected item disappears.
    pub pos: [usize; 3],
    /// Selection in history mode, over `visible_runs()`.
    pub run_idx: usize,
    /// `h`: rail shows the full run list instead of the board.
    pub history: bool,
    /// `?`: about overlay.
    pub about: bool,
    /// Initial selection made (waits for the first non-empty snapshot).
    pub picked: bool,
    /// Last draw was < 60 cols: no rail, so keys act as if Session is focused.
    pub narrow: bool,
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
        now_run: None,
        attn_sel: None,
        next_skill: None,
        pos: [0; 3],
        run_idx: 0,
        history: false,
        about: false,
        picked: false,
        narrow: false,
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

/// Pull a fresh snapshot, then new events of displayed NOW runs (tool line) and the selected run.
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
    let mut ids: Vec<String> = ui::board(&m.snap).now.iter().map(|r| r.id.clone()).collect();
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
    fallback(st, ui);
    ui.pane = ui.rail;
    ui.picked = true;
}

/// Select the first running run, else the newest failure, else the first NEXT row.
fn fallback(st: &Snapshot, ui: &mut Ui) {
    let b = ui::board(st);
    if let Some(r) = b.now.first() {
        (ui.rail, ui.now_run) = (Pane::Now, Some(r.id.clone()));
    } else if let Some(a) = b.attn.iter().find(|a| matches!(a, ui::Attn::Run(_))).or(b.attn.first()) {
        (ui.rail, ui.attn_sel) = (Pane::Attention, Some(a.key()));
    } else {
        (ui.rail, ui.next_skill) = (Pane::Next, b.next.first().map(|v| v.name.clone()));
    }
}

fn slot(p: Pane) -> usize {
    match p {
        Pane::Now => 0,
        Pane::Attention => 1,
        _ => 2,
    }
}

/// Resolve the rail's selection; re-anchor it when its item is gone.
/// A finished NOW run moves to its skill's NEXT row; anything else clamps to the nearest row.
fn settle(st: &Snapshot, ui: &mut Ui) {
    if ui.history {
        return;
    }
    let b = ui::board(st);
    if ui.rail == Pane::Now && ui::sel_index(&b, ui).is_none() {
        let done = ui.now_run.as_ref().and_then(|id| st.runs.iter().find(|r| r.id == *id)).filter(|r| r.status != RunStatus::Running);
        if let Some(r) = done {
            ui.next_skill = Some(r.skill.clone());
            ui.rail = Pane::Next;
            if ui.pane == Pane::Now {
                ui.pane = Pane::Next;
            }
        }
    }
    let n = ui::section_len(&b, ui.rail);
    if n == 0 {
        let was_rail = ui.pane == ui.rail;
        fallback(st, ui);
        if was_rail {
            ui.pane = ui.rail;
        }
        return settle_pos(&ui::board(st), ui);
    }
    if ui::sel_index(&b, ui).is_none() {
        let i = ui.pos[slot(ui.rail)].min(n - 1);
        select(&b, ui, i);
    }
    settle_pos(&b, ui);
}

fn settle_pos(b: &ui::Board, ui: &mut Ui) {
    if let Some(i) = ui::sel_index(b, ui) {
        ui.pos[slot(ui.rail)] = i;
    }
}

/// Point the rail's selection at row `i` of its section.
fn select(b: &ui::Board, ui: &mut Ui, i: usize) {
    match ui.rail {
        Pane::Now => ui.now_run = b.now.get(i).map(|r| r.id.clone()),
        Pane::Attention => ui.attn_sel = b.attn.get(i).map(|a| a.key()),
        _ => ui.next_skill = b.next.get(i).map(|v| v.name.clone()),
    }
    ui.pos[slot(ui.rail)] = i;
}

/// Board sections that have selectable rows, in tab order.
fn sections(st: &Snapshot) -> Vec<Pane> {
    let b = ui::board(st);
    [Pane::Now, Pane::Attention, Pane::Next].into_iter().filter(|p| ui::section_len(&b, *p) > 0).collect()
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
        settle(st, ui);
        reset_session(ui);
    }
}

fn reset_session(ui: &mut Ui) {
    ui.session_scroll = 0;
    ui.follow = true;
}

/// Move the focused rail selection by `d`, clamped to the rows shown.
fn move_sel(ui: &mut Ui, st: &Snapshot, d: isize) {
    if ui.history {
        ui.run_idx = ui.run_idx.saturating_add_signed(d).min(ui::visible_runs(st, ui).len().saturating_sub(1));
    } else {
        let b = ui::board(st);
        let n = ui::section_len(&b, ui.rail);
        let cur = ui::sel_index(&b, ui).unwrap_or(0);
        select(&b, ui, cur.saturating_add_signed(d).min(n.saturating_sub(1)));
    }
    reset_session(ui);
}

async fn handle_key(code: KeyCode, ui: &mut Ui, m: &mut Model, backend: &mut Backend) {
    if ui.about {
        if matches!(code, KeyCode::Esc | KeyCode::Char('?')) {
            ui.about = false;
        }
        return;
    }
    // Narrow: no rail drawn, so the session has focus and tab has nowhere to go.
    let pane = if ui.narrow { Pane::Session } else { ui.pane };
    match code {
        KeyCode::Tab | KeyCode::BackTab if ui.narrow => {}
        KeyCode::Tab => cycle(ui, &m.snap, false),
        KeyCode::BackTab => cycle(ui, &m.snap, true),
        KeyCode::Char('j') | KeyCode::Down if pane == Pane::Session => {
            ui.follow = false;
            ui.session_scroll = ui.session_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up if pane == Pane::Session => {
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
        KeyCode::Enter => match (ui.history, pane) {
            (_, Pane::Session) => {}
            // NEXT skill: toggle the history filter and show history.
            (false, Pane::Next) => {
                let name = ui.next_skill.clone();
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
            } else if m.snap.skills.iter().any(|v| v.name == name) {
                ui.next_skill = Some(name);
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
