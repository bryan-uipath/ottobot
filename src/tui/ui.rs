//! Drawing. Status strip / left rail (board or history) │ session or log / footer; `?` overlay.

use super::{Model, Pane, Ui};
use crate::otto;
use crate::proto::{SkillSnap, Snapshot};
use crate::run::{RunEvent, RunMeta, RunStatus};
use chrono::Local;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Rgb(0, 102, 217); // Otto blue
const ORANGE: Color = Color::Rgb(232, 68, 26);
const DIM: Color = Color::DarkGray;
const SEL_BG: Color = Color::Rgb(30, 30, 40);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Rail width including 1-col padding each side (44 content).
const RAIL_W: u16 = 46;
const NAME_W: usize = 14;
const NOW_CAP: usize = 3;
const ATTN_CAP: usize = 5;
const BAR_W: usize = 20;

fn spin(frame: usize) -> &'static str {
    SPINNER[frame % SPINNER.len()]
}

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// ATTENTION row: a recent failed run, or a skill/file that failed to parse.
pub enum Attn<'a> {
    Run(&'a RunMeta),
    Invalid { label: String, err: String, skill: Option<String> },
}

/// Board rows, already capped; indices in `Ui` point into these.
pub struct Board<'a> {
    pub now: Vec<&'a RunMeta>,
    pub now_more: usize,
    pub attn: Vec<Attn<'a>>,
    pub attn_more: usize,
    pub next: Vec<&'a SkillSnap>,
    /// Failed runs in the last 24h, uncapped.
    pub failed: usize,
}

pub fn board(st: &Snapshot) -> Board<'_> {
    let mut now: Vec<&RunMeta> = st.runs.iter().filter(|r| r.status == RunStatus::Running).collect();
    let now_more = now.len().saturating_sub(NOW_CAP);
    now.truncate(NOW_CAP);
    // Parse errors first: they never clear on their own.
    let mut attn: Vec<Attn> = st
        .skills
        .iter()
        .filter_map(|v| v.trigger.as_ref().err().map(|e| Attn::Invalid { label: v.name.clone(), err: e.clone(), skill: Some(v.name.clone()) }))
        .collect();
    attn.extend(st.skill_errors.iter().map(|(p, e)| Attn::Invalid { label: file_name(p), err: e.clone(), skill: None }));
    let since = Local::now() - chrono::Duration::hours(24);
    let failed: Vec<&RunMeta> = st.runs.iter().filter(|r| matches!(r.status, RunStatus::Failed(_)) && r.started_at >= since).collect();
    let n_failed = failed.len();
    attn.extend(failed.into_iter().map(Attn::Run));
    let attn_more = attn.len().saturating_sub(ATTN_CAP);
    attn.truncate(ATTN_CAP);
    let mut next: Vec<&SkillSnap> = st.skills.iter().collect();
    next.sort_by_key(|v| (v.next_fire.is_none(), v.next_fire));
    Board { now, now_more, attn, attn_more, next, failed: n_failed }
}

/// Runs shown in history mode, honoring the skill filter. Newest first.
pub fn visible_runs<'a>(st: &'a Snapshot, ui: &Ui) -> Vec<&'a RunMeta> {
    st.runs.iter().filter(|r| ui.run_filter.as_deref().map_or(true, |f| r.skill == f)).collect()
}

/// Skill's active run, else its newest.
fn latest_run<'a>(st: &'a Snapshot, v: &SkillSnap) -> Option<&'a RunMeta> {
    v.active_run.as_ref().and_then(|id| st.runs.iter().find(|r| r.id == *id)).or_else(|| st.runs.iter().find(|r| r.skill == v.name))
}

/// Run shown in the session pane.
pub fn selected_run<'a>(st: &'a Snapshot, ui: &Ui) -> Option<&'a RunMeta> {
    if ui.history {
        return visible_runs(st, ui).get(ui.run_idx).copied();
    }
    let b = board(st);
    match ui.rail {
        Pane::Now => b.now.get(ui.now_idx).copied(),
        Pane::Attention => match b.attn.get(ui.att_idx) {
            Some(Attn::Run(r)) => Some(*r),
            _ => None,
        },
        Pane::Next => b.next.get(ui.next_idx).and_then(|v| latest_run(st, v)),
        Pane::Session => None,
    }
}

/// Skill `r` fires: the selected skill, or the selected run's skill.
pub fn selected_skill(st: &Snapshot, ui: &Ui) -> Option<String> {
    if !ui.history {
        let b = board(st);
        match (ui.rail, b.attn.get(ui.att_idx)) {
            (Pane::Next, _) => return b.next.get(ui.next_idx).map(|v| v.name.clone()),
            (Pane::Attention, Some(Attn::Invalid { skill, .. })) => return skill.clone(),
            _ => {}
        }
    }
    selected_run(st, ui).map(|r| r.skill.clone())
}

pub fn draw(f: &mut Frame, m: &Model, ui: &mut Ui) {
    let area = f.area();
    let [strip, _, body, footer] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    draw_strip(f, m, ui, strip);
    draw_footer(f, ui, footer);
    if area.width < 60 {
        draw_right(f, m, ui, body);
    } else {
        let [rail, rule, _, right] = Layout::horizontal([Constraint::Length(RAIL_W), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(body);
        if ui.history {
            draw_history(f, &m.snap, ui, rail);
        } else {
            draw_board(f, m, ui, rail);
        }
        let rule_style = Style::default().fg(if ui.pane == Pane::Session { ACCENT } else { DIM });
        f.render_widget(Paragraph::new(vec![Line::from("│"); rule.height as usize]).style(rule_style), rule);
        draw_right(f, m, ui, Rect { width: right.width.saturating_sub(1), ..right });
    }
    if ui.about {
        draw_about(f, &m.snap, area);
    }
}

fn draw_strip(f: &mut Frame, m: &Model, ui: &Ui, area: Rect) {
    let st = &m.snap;
    let b = board(st);
    let running = st.runs.iter().filter(|r| r.status == RunStatus::Running).count();
    let mut left = vec![Span::styled(" ottobot", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw("   ")];
    if running > 0 {
        left.extend([Span::styled(format!("{} ", spin(ui.frame)), Style::default().fg(ACCENT)), Span::raw(format!("{running} running"))]);
    } else {
        left.push(Span::styled("0 running", dim()));
    }
    left.push(Span::raw("   "));
    left.push(if b.failed > 0 { Span::styled(format!("✗ {} failed", b.failed), Style::default().fg(ORANGE)) } else { Span::styled("0 failed", dim()) });
    if let Some(v) = st.skills.iter().filter(|v| v.next_fire.is_some()).min_by_key(|v| v.next_fire) {
        let when = v.next_fire.map(|t| rel(t - Local::now())).unwrap_or_default();
        left.push(Span::raw(format!("   next {} {when}", v.name)));
    }
    let (text, style) = match (&m.action_error, &m.error, st.daemon_pid) {
        (Some((_, e)), _, _) => (format!("✗ {e}"), Style::default().fg(ORANGE)),
        (None, Some(_), Some(_)) => ("✗ daemon disconnected · retrying".to_string(), Style::default().fg(ORANGE)),
        (None, Some(e), None) => (format!("✗ {e}"), Style::default().fg(ORANGE)),
        (None, None, pid) => {
            let who = pid.map(|p| format!("daemon {p}")).unwrap_or_else(|| "in-process".into());
            (format!("{who} · {}", if st.caffeinated { "caffeinated" } else { "not caffeinated" }), dim())
        }
    };
    let left = Line::from(left);
    // Right side yields to the left: truncate, or drop if nothing useful fits.
    let room = (area.width as usize).saturating_sub(left.width() + 3);
    let text = if room >= 8 { fit(&text, room) } else { String::new() };
    let rw = text.chars().count() as u16 + 1;
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(left), l);
    f.render_widget(Paragraph::new(Span::styled(text, style)), r);
}

/// `NAME ────…` across `w` cols.
fn header(name: &str, w: usize) -> Line<'static> {
    let rule = "─".repeat(w.saturating_sub(name.chars().count() + 2));
    Line::from(vec![Span::styled(name.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::styled(format!(" {rule}"), dim())])
}

fn dim_line(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(s.into(), dim()))
}

fn more(lines: &mut Vec<Line<'static>>, n: usize) {
    if n > 0 {
        lines.push(dim_line(format!("+{n} more")));
    }
}

fn draw_board(f: &mut Frame, m: &Model, ui: &Ui, area: Rect) {
    let st = &m.snap;
    let b = board(st);
    let w = area.width.saturating_sub(2) as usize;
    let now = Local::now();
    let mut lines: Vec<Line> = Vec::new();
    let mut sel = None;

    lines.push(header("NOW", w));
    if b.now.is_empty() {
        lines.push(dim_line("none"));
    }
    for (i, r) in b.now.iter().enumerate() {
        if ui.rail == Pane::Now && i == ui.now_idx {
            sel = Some(lines.len());
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", spin(ui.frame)), Style::default().fg(ACCENT)),
            Span::styled(col(&r.skill, NAME_W), bold()),
            Span::raw(fit(&format!("{} · {}", clock(now - r.started_at), runner_model(r, None)), w.saturating_sub(NAME_W + 2))),
        ]));
        let tool = m.events.get(&r.id).and_then(|es| {
            es.iter().rev().find_map(|e| match e {
                RunEvent::ToolUse { name, summary } => Some(format!("  ⚙ {name}  {summary}")),
                _ => None,
            })
        });
        if let Some(t) = tool {
            lines.push(dim_line(fit(&t, w)));
        }
    }
    more(&mut lines, b.now_more);
    lines.push(Line::from(""));

    lines.push(header("ATTENTION", w));
    if b.attn.is_empty() {
        lines.push(dim_line("none"));
    }
    for (i, a) in b.attn.iter().enumerate() {
        if ui.rail == Pane::Attention && i == ui.att_idx {
            sel = Some(lines.len());
        }
        let (name, rest) = match a {
            Attn::Run(r) => {
                let why = match &r.status {
                    RunStatus::Failed(why) => why.lines().next().unwrap_or_default().to_string(),
                    _ => String::new(),
                };
                // Reason over clock time: the session header has the timestamp; the reason is why it's here.
                let tail = format!(" · {}", rel_dur(r.finished_at.unwrap_or(now) - r.started_at));
                let room = w.saturating_sub(NAME_W + 2 + tail.chars().count());
                (r.skill.clone(), format!("{}{tail}", fit(&why, room)))
            }
            Attn::Invalid { label, err, .. } => (label.clone(), fit(err.lines().next().unwrap_or_default(), w.saturating_sub(NAME_W + 2))),
        };
        lines.push(Line::from(vec![Span::styled("✗ ", Style::default().fg(ORANGE)), Span::styled(col(&name, NAME_W), bold()), Span::styled(rest, dim())]));
    }
    more(&mut lines, b.attn_more);
    lines.push(Line::from(""));

    lines.push(header("QUOTA", w));
    for (name, health) in &st.runner_health {
        let usage = st.runner_usage.iter().find(|(n, _)| n == name).map(|(_, u)| u);
        match (health, usage) {
            (Err(e), _) => lines.push(Line::from(Span::styled(fit(&format!("✗ {name} {e}"), w), Style::default().fg(ORANGE)))),
            (Ok(_), None) => lines.push(dim_line(format!("{name:<6}  …"))),
            (Ok(_), Some(Err(_))) => lines.push(dim_line(format!("{name:<6}  unavailable"))),
            (Ok(_), Some(Ok(ws))) if ws.is_empty() => lines.push(dim_line(format!("{name:<6}  no usage data"))),
            (Ok(_), Some(Ok(ws))) => {
                for u in ws {
                    let used = u.used_percent.clamp(0.0, 100.0);
                    let filled = ((used / 100.0 * BAR_W as f64).round() as usize).min(BAR_W);
                    let color = if used >= 80.0 { ORANGE } else { ACCENT };
                    lines.push(Line::from(vec![
                        Span::raw(format!("{name:<6} {:<4} ", u.label)),
                        Span::styled("█".repeat(filled), Style::default().fg(color)),
                        Span::styled("░".repeat(BAR_W - filled), dim()),
                        Span::raw(format!("  {:>3.0}% left", 100.0 - used)),
                    ]));
                }
            }
        }
    }
    lines.push(Line::from(""));

    lines.push(header("NEXT", w));
    if b.next.is_empty() {
        lines.push(dim_line("none"));
    }
    let whens: Vec<String> = b
        .next
        .iter()
        .map(|v| {
            let check = if v.trigger.as_ref().is_ok_and(|t| t.starts_with("event")) { "check " } else { "" };
            v.next_fire.map(|t| format!("{check}{}", rel(t - now))).unwrap_or_else(|| "no schedule".into())
        })
        .collect();
    let when_w = whens.iter().map(|s| s.chars().count() + 2).max().unwrap_or(0).max(12);
    for (i, (v, when)) in b.next.iter().zip(&whens).enumerate() {
        if ui.rail == Pane::Next && i == ui.next_idx {
            sel = Some(lines.len());
        }
        let glyph = match (&v.active_run, &v.trigger) {
            (Some(_), _) => Span::styled(format!("{} ", spin(ui.frame)), Style::default().fg(ACCENT)),
            (None, Err(_)) => Span::styled("✗ ", Style::default().fg(ORANGE)),
            (None, Ok(_)) => Span::styled("○ ", dim()),
        };
        let last = match (&v.active_run, v.last_fired) {
            (Some(_), _) => "running".to_string(),
            (None, Some(t)) => format!("last {} ago", short(now - t)),
            (None, None) => "never ran".to_string(),
        };
        let rest = format!("{when:<when_w$}{last}");
        let used = 2 + NAME_W + rest.chars().count();
        let mut spans = vec![glyph, Span::styled(col(&v.name, NAME_W), bold()), Span::raw(rest)];
        // Scheduler note only if it fits whole.
        if !v.note.is_empty() && used + 2 + v.note.chars().count() <= w {
            spans.push(Span::styled(format!("  {}", v.note), dim()));
        }
        lines.push(Line::from(spans));
    }

    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    f.render_widget(Paragraph::new(lines), inner);
    if let Some(i) = sel {
        mark(f, area, i as u16, ui.pane != Pane::Session);
    }
}

/// Highlight rail row `row` (relative to `area`); accent marker in the left pad when the rail has focus.
fn mark(f: &mut Frame, area: Rect, row: u16, focused: bool) {
    if row >= area.height || area.width == 0 {
        return;
    }
    let y = area.y + row;
    let buf = f.buffer_mut();
    buf.set_style(Rect { y, height: 1, ..area }, Style::default().bg(SEL_BG));
    if focused {
        buf[(area.x, y)].set_symbol("▌").set_fg(ACCENT);
    }
}

fn draw_history(f: &mut Frame, st: &Snapshot, ui: &mut Ui, area: Rect) {
    let runs = visible_runs(st, ui);
    ui.run_idx = ui.run_idx.min(runs.len().saturating_sub(1));
    let w = area.width.saturating_sub(2) as usize;
    let title = match &ui.run_filter {
        Some(s) => format!("HISTORY · {s} ({})", runs.len()),
        None => format!("HISTORY ({})", runs.len()),
    };
    let [head, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let pad = |r: Rect| Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r };
    f.render_widget(Paragraph::new(header(&title, w)), pad(head));
    if runs.is_empty() {
        f.render_widget(Paragraph::new(dim_line("no runs")), pad(list));
        return;
    }
    let items: Vec<ListItem> = runs
        .iter()
        .map(|r| {
            let (dot, color) = match &r.status {
                RunStatus::Running => (spin(ui.frame), ACCENT),
                RunStatus::Succeeded => ("✓", Color::Green),
                RunStatus::Failed(_) => ("✗", ORANGE),
            };
            let dur = r.finished_at.unwrap_or_else(Local::now) - r.started_at;
            let cost = r.cost_usd.map(|c| format!(" · ${c:.2}")).unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(col(&r.skill, NAME_W), bold()),
                // When · duration · cost; runner/model live in the session header.
                Span::styled(fit(&format!("{}  {}{cost}", r.started_at.format("%m-%d %H:%M"), rel_dur(dur)), w.saturating_sub(NAME_W + 2)), dim()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(ui.run_idx));
    f.render_stateful_widget(List::new(items), pad(list), &mut state);
    mark(f, list, (ui.run_idx - state.offset().min(ui.run_idx)) as u16, ui.pane != Pane::Session);
}

fn draw_right(f: &mut Frame, m: &Model, ui: &mut Ui, area: Rect) {
    if ui.show_log {
        draw_log(f, &m.snap, area);
    } else {
        draw_session(f, m, ui, area);
    }
}

fn draw_session(f: &mut Frame, m: &Model, ui: &mut Ui, area: Rect) {
    let st = &m.snap;
    let Some(run) = selected_run(st, ui) else {
        // Parse error selected in ATTENTION: show it in full.
        let b = board(st);
        let text = match (ui.history, ui.rail, b.attn.get(ui.att_idx)) {
            (false, Pane::Attention, Some(Attn::Invalid { label, err, .. })) => {
                let mut l = vec![Line::from(Span::styled(format!("✗ {label}"), Style::default().fg(ORANGE).add_modifier(Modifier::BOLD))), Line::from("")];
                l.extend(err.lines().map(|s| Line::from(s.to_string())));
                Text::from(l)
            }
            _ => Text::from(dim_line("no runs yet · press r to run the selected skill")),
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    };
    let now = Local::now();
    let events = m.events.get(&run.id).map(Vec::as_slice).unwrap_or_default();
    let init_model = events.iter().find_map(|e| match e {
        RunEvent::Init { model, .. } => model.clone(),
        _ => None,
    });
    let elapsed = clock(run.finished_at.unwrap_or(now) - run.started_at);
    let (status, color) = match &run.status {
        RunStatus::Running => (format!("{} running {elapsed}", spin(ui.frame)), ACCENT),
        RunStatus::Succeeded => (format!("✓ succeeded {elapsed}"), Color::Reset),
        RunStatus::Failed(_) => (format!("✗ failed {elapsed}"), ORANGE),
    };
    let head = Line::from(vec![
        Span::styled(run.skill.clone(), bold()),
        Span::styled(" · ", dim()),
        Span::styled(status, Style::default().fg(color)),
        Span::styled(" · ", dim()),
        Span::raw(runner_model(run, init_model.as_deref())),
    ]);
    // cwd right-aligned, cut from the left to fit; dropped if under 10 cols remain.
    let room = (area.width as usize).saturating_sub(head.width() + 3);
    let cwd = if run.cwd.as_os_str().is_empty() || room < 10 { String::new() } else { format!("⌂ {}", fit_left(&short_path(&run.cwd), room - 2)) };
    let cw = cwd.chars().count() as u16;
    let show_cwd = cw > 0;
    let [h, _, body] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let [hl, hr] = Layout::horizontal([Constraint::Min(0), Constraint::Length(if show_cwd { cw } else { 0 })]).areas(h);
    f.render_widget(Paragraph::new(head), hl);
    if show_cwd {
        f.render_widget(Paragraph::new(Span::styled(cwd, dim())), hr);
    }

    let mut lines: Vec<Line> = Vec::new();
    for e in events {
        match e {
            RunEvent::Init { model, .. } => {
                if let Some(model) = model {
                    lines.push(dim_line(format!("▸ {model}")));
                }
            }
            RunEvent::Text(t) => {
                for l in t.lines() {
                    lines.push(Line::from(l.to_string()));
                }
                lines.push(Line::from(""));
            }
            RunEvent::ToolUse { name, summary } => lines.push(Line::from(vec![Span::styled(format!("⚙ {name} "), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::styled(summary.clone(), dim())])),
            RunEvent::ToolResult { summary, is_error } => lines.push(Line::from(Span::styled(format!("  ↳ {summary}"), Style::default().fg(if *is_error { ORANGE } else { DIM })))),
            RunEvent::Usage { input, output, cost_usd } => lines.push(dim_line(format!("Σ {input} in / {output} out{}", cost_usd.map(|c| format!(" · ${c:.3}")).unwrap_or_default()))),
            RunEvent::Stderr(s) => lines.push(Line::from(Span::styled(format!("! {s}"), Style::default().fg(ORANGE)))),
            RunEvent::Raw(s) => lines.push(dim_line(s.clone())),
            RunEvent::Done { ok, summary } => {
                lines.push(Line::from(""));
                let style = Style::default().fg(if *ok { Color::Reset } else { ORANGE }).add_modifier(Modifier::BOLD);
                lines.push(Line::from(Span::styled(if *ok { "✓ done" } else { "✗ failed" }, style)));
                for l in summary.lines() {
                    lines.push(Line::from(l.to_string()));
                }
            }
        }
    }
    if let RunStatus::Failed(why) = &run.status {
        lines.push(Line::from(Span::styled(format!("✗ {why}"), Style::default().fg(ORANGE))));
    }
    let inner_h = body.height as usize;
    let inner_w = body.width.max(1) as usize;
    // Estimate wrapped height so follow-mode pins to the bottom.
    let total: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum();
    let max_scroll = total.saturating_sub(inner_h).min(u16::MAX as usize) as u16;
    ui.session_scroll = if ui.follow { max_scroll } else { ui.session_scroll.min(max_scroll) };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((ui.session_scroll, 0)), body);
}

fn draw_log(f: &mut Frame, st: &Snapshot, area: Rect) {
    let [h, _, body] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(Paragraph::new(header("SCHEDULER LOG", h.width as usize)), h);
    let lines: Vec<Line> = st.log.iter().rev().take(body.height as usize).rev().map(|(t, m)| Line::from(vec![Span::styled(format!("{} ", t.format("%H:%M:%S")), dim()), Span::raw(m.clone())])).collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn draw_footer(f: &mut Frame, ui: &Ui, area: Rect) {
    let keys = [
        ("tab", "pane"),
        ("j/k", "move"),
        ("enter", "open"),
        ("r", "run now"),
        ("R", "reload"),
        ("h", if ui.history { "board" } else { "history" }),
        ("l", if ui.show_log { "session" } else { "log" }),
        ("?", "about"),
        ("q", "quit"),
    ];
    let spans: Vec<Span> = keys.iter().flat_map(|(k, v)| [Span::raw(format!(" {k}")), Span::styled(format!(" {v} "), dim())]).collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Centered `?` overlay: Otto art beside paths, runners, daemon, and keys.
fn draw_about(f: &mut Frame, st: &Snapshot, area: Rect) {
    let now = Local::now();
    // Art beside the info when there's room; info alone otherwise.
    let side = area.width >= 112;
    let w = (if side { otto::WIDTH + 71 } else { 70 }).min(area.width);
    let path_w = (w as usize).saturating_sub(if side { otto::WIDTH as usize + 16 } else { 15 });
    let mut info = vec![
        Line::from(Span::styled("ottobot", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
        Line::from(""),
        kv("root", fit_left(&short_path(&st.root), path_w)),
        kv("store", fit_left(&short_path(&st.store_dir), path_w)),
    ];
    for (name, h) in &st.runner_health {
        info.push(match h {
            Ok(v) => kv(name, v.clone()),
            Err(e) => Line::from(vec![Span::styled(format!("{name:<10} "), dim()), Span::styled(format!("✗ {e}"), Style::default().fg(ORANGE))]),
        });
    }
    info.push(kv("daemon", st.daemon_pid.map(|p| format!("pid {p}")).unwrap_or_else(|| "in-process".into())));
    info.push(kv("next tick", st.next_tick.map(|t| format!("{} ({})", t.format("%H:%M:%S"), rel(t - now))).unwrap_or("—".into())));
    info.push(kv("awake", if st.caffeinated { "caffeinated".into() } else { "not caffeinated".into() }));
    info.push(Line::from(""));
    let keys: [&[(&str, &str)]; 4] = [
        &[("tab", "cycle panes"), ("j/k", "move / scroll"), ("g/G", "top / follow")],
        &[("enter", "open · filter"), ("esc", "clear filter"), ("r", "run now")],
        &[("R", "reload"), ("h", "history"), ("l", "scheduler log")],
        &[("?", "close"), ("q", "quit")],
    ];
    for row in keys {
        info.push(Line::from(row.iter().flat_map(|(k, v)| [Span::raw(format!("{k:<6}")), Span::styled(format!("{v:<16}"), dim())]).collect::<Vec<_>>()));
    }
    let art_h = if side { otto::height() } else { 0 };
    let h = (art_h.max(info.len() as u16) + 2).min(area.height);
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let block = Block::bordered().border_style(dim());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let art_w = if side { otto::WIDTH } else { 0 };
    let [_, art, _, rest] = Layout::horizontal([Constraint::Length(1), Constraint::Length(art_w), Constraint::Length(if side { 2 } else { 0 }), Constraint::Min(0)]).areas(inner);
    if side {
        f.render_widget(Paragraph::new(otto::text()), art);
    }
    f.render_widget(Paragraph::new(info), rest);
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<10} "), dim()), Span::raw(v)])
}

/// `runner/model`; model from the run meta, else the session's Init event.
fn runner_model(r: &RunMeta, init: Option<&str>) -> String {
    match r.model.as_deref().or(init) {
        Some(m) => format!("{}/{m}", r.runner),
        None => r.runner.clone(),
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Truncate to `n` chars, ending in `…` when cut.
fn fit(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    if n > 0 {
        out.push('…');
    }
    out
}

/// Truncate to `n` chars from the left, e.g. `…/scratchpad/dskills`.
fn fit_left(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len <= n {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(len + 1 - n.max(1)).collect::<String>())
}

/// Fixed-width column: fit to `n - 1`, pad to `n` (always one space gap).
fn col(s: &str, n: usize) -> String {
    format!("{:<n$}", fit(s, n.saturating_sub(1)))
}

/// `~`-relative path for display.
fn short_path(p: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    }
}

fn rel(d: chrono::Duration) -> String {
    if d.num_seconds() < 0 {
        return "now".into();
    }
    format!("in {}", short(d))
}

/// Coarse duration for schedule times, e.g. `49s`, `14m`, `2h11m`, `3d4h`.
fn short(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => rel_dur(d),
    }
}

/// Elapsed wall clock, e.g. `0:42`, `1:02:03`.
fn clock(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn rel_dur(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{}h", s / 86400, (s % 86400) / 3600),
    }
}
