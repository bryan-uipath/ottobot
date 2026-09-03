//! Drawing. Header (Otto + status) / left: skills over runs / right: session or log.

use super::{Pane, Ui};
use crate::otto;
use crate::run::{RunEvent, RunStatus};
use crate::state::AppState;
use chrono::Local;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Rgb(0, 102, 217); // Otto blue
const GOLD: Color = Color::Rgb(243, 182, 64);
const ORANGE: Color = Color::Rgb(232, 68, 26);
const DIM: Color = Color::DarkGray;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spin(frame: usize) -> &'static str {
    SPINNER[frame % SPINNER.len()]
}

pub fn draw(f: &mut Frame, st: &AppState, ui: &mut Ui) {
    let area = f.area();
    let header_h = otto::height().min(area.height / 3).max(6) + 1;
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(header_h), Constraint::Min(5), Constraint::Length(1)]).areas(area);
    draw_header(f, st, ui, header);
    let [left, right] = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).areas(body);
    let [skills, runs] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(left);
    draw_skills(f, st, ui, skills);
    draw_runs(f, st, ui, runs);
    if ui.show_log {
        draw_log(f, st, right);
    } else {
        draw_session(f, st, ui, right);
    }
    draw_footer(f, ui, footer);
}

fn draw_header(f: &mut Frame, st: &AppState, ui: &Ui, area: Rect) {
    let art_w = otto::WIDTH.min(area.width);
    let [art, info] = Layout::horizontal([Constraint::Length(art_w + 2), Constraint::Min(10)]).areas(area);
    f.render_widget(Paragraph::new(otto::text()), art);

    let now = Local::now();
    let running = st.runs.iter().filter(|r| r.meta.status == RunStatus::Running).count();
    let next_fire = st.skills.iter().filter_map(|v| v.next_fire).min();
    let mut lines = vec![
        Line::from(vec![Span::styled(" ottobot", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::styled(format!("  {}", st.root.display()), Style::default().fg(DIM))]),
        Line::from(""),
        kv("skills", format!("{}  ({} invalid)", st.skills.len(), st.skill_errors.len())),
        kv("running", if running > 0 { format!("{} {running}", spin(ui.frame)) } else { "0".into() }),
        kv("next tick", st.next_tick.map(|t| format!("{} ({})", t.format("%H:%M:%S"), rel(t - now))).unwrap_or("—".into())),
        kv("next fire", next_fire.map(|t| format!("{} ({})", t.format("%a %H:%M"), rel(t - now))).unwrap_or("—".into())),
        kv("awake", if st.caffeinated { "caffeinated ☕".to_string() } else { "not caffeinated".to_string() }),
    ];
    for (name, h) in &st.runner_health {
        lines.push(match h {
            Ok(v) => kv(name, format!("✓ {v}")),
            Err(e) => Line::from(vec![Span::styled(format!(" {name:<10} "), Style::default().fg(DIM)), Span::styled(format!("✗ {e}"), Style::default().fg(ORANGE))]),
        });
    }
    f.render_widget(Paragraph::new(lines), info);
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!(" {k:<10} "), Style::default().fg(DIM)), Span::raw(v)])
}

fn draw_skills(f: &mut Frame, st: &AppState, ui: &mut Ui, area: Rect) {
    let items: Vec<ListItem> = st
        .skills
        .iter()
        .map(|v| {
            let (dot, color) = match (&v.trigger, &v.active_run) {
                (Err(_), _) => ("✗", ORANGE),
                (_, Some(_)) => (spin(ui.frame), GOLD),
                _ => ("○", DIM),
            };
            let when = v.next_fire.map(|t| format!("next {}", rel(t - Local::now()))).unwrap_or_else(|| "no schedule".into());
            let last = v.last_fired.map(|t| format!("last {}", rel_past(Local::now() - t))).unwrap_or_else(|| "never ran".into());
            let trig = v.trigger.as_ref().map(|t| t.summary()).unwrap_or_else(|e| e.clone());
            ListItem::new(vec![
                Line::from(vec![Span::styled(format!("{dot} "), Style::default().fg(color)), Span::styled(v.skill.name.clone(), Style::default().add_modifier(Modifier::BOLD))]),
                Line::from(Span::styled(format!("  {trig}"), Style::default().fg(DIM))),
                Line::from(vec![Span::styled(format!("  {when} · {last}"), Style::default().fg(DIM)), Span::styled(if v.note.is_empty() { String::new() } else { format!(" · {}", v.note) }, Style::default().fg(GOLD))]),
            ])
        })
        .collect();
    let mut errs: Vec<ListItem> = st
        .skill_errors
        .iter()
        .map(|(p, e)| ListItem::new(vec![Line::from(Span::styled(format!("✗ {}", p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()), Style::default().fg(ORANGE))), Line::from(Span::styled(format!("  {e}"), Style::default().fg(DIM)))]))
        .collect();
    let mut all = items;
    all.append(&mut errs);
    let list = List::new(all)
        .block(pane_block("skills", ui.pane == Pane::Skills))
        .highlight_style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let mut state = ListState::default().with_selected(Some(ui.skill_idx));
    f.render_stateful_widget(list, area, &mut state);
}

/// Runs shown in the runs pane, honoring the skill filter. Newest first.
pub fn visible_runs<'a>(st: &'a AppState, ui: &Ui) -> Vec<&'a crate::run::Run> {
    st.runs.iter().filter(|r| ui.run_filter.as_deref().map_or(true, |f| r.meta.skill == f)).collect()
}

fn draw_runs(f: &mut Frame, st: &AppState, ui: &mut Ui, area: Rect) {
    let runs = visible_runs(st, ui);
    let items: Vec<ListItem> = runs
        .iter()
        .map(|r| {
            let (dot, color) = match &r.meta.status {
                RunStatus::Running => (spin(ui.frame), GOLD),
                RunStatus::Succeeded => ("✓", Color::Green),
                RunStatus::Failed(_) => ("✗", ORANGE),
            };
            let dur = r.meta.finished_at.unwrap_or_else(Local::now) - r.meta.started_at;
            let cost = r.meta.cost_usd.map(|c| format!(" ${c:.2}")).unwrap_or_default();
            ListItem::new(vec![
                Line::from(vec![Span::styled(format!("{dot} "), Style::default().fg(color)), Span::styled(r.meta.skill.clone(), Style::default().add_modifier(Modifier::BOLD)), Span::styled(format!("  {}", r.meta.started_at.format("%m-%d %H:%M")), Style::default().fg(DIM))]),
                Line::from(Span::styled(format!("  {} · {}{}{}", rel_dur(dur), r.meta.runner, r.meta.model.as_ref().map(|m| format!("/{m}")).unwrap_or_default(), cost), Style::default().fg(DIM))),
                Line::from(Span::styled(if r.meta.cwd.as_os_str().is_empty() { String::new() } else { format!("  ⌂ {}", short_path(&r.meta.cwd)) }, Style::default().fg(DIM))),
            ])
        })
        .collect();
    let title = match &ui.run_filter {
        Some(f) => format!("runs · {f} ({})", runs.len()),
        None => format!("runs ({})", runs.len()),
    };
    let list = List::new(items).block(pane_block(&title, ui.pane == Pane::Runs)).highlight_style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let mut state = ListState::default().with_selected(if runs.is_empty() { None } else { Some(ui.run_idx) });
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_session(f: &mut Frame, st: &AppState, ui: &mut Ui, area: Rect) {
    let Some(run) = visible_runs(st, ui).get(ui.run_idx).copied() else {
        f.render_widget(Paragraph::new("no runs yet · press r on a skill to run it now").fg(DIM).block(pane_block("session", ui.pane == Pane::Session)), area);
        return;
    };
    let status = match run.meta.status {
        RunStatus::Running => format!("{} running", spin(ui.frame)),
        _ => status_str(&run.meta.status).to_string(),
    };
    let title = format!("session · {} · {} · ⌂ {}", run.meta.id, status, short_path(&run.meta.cwd));
    let log_path = short_path(&st.store_dir.join("runs").join(format!("{}.jsonl", run.meta.id)));
    let mut lines: Vec<Line> = Vec::new();
    for e in &run.events {
        match e {
            RunEvent::Init { session, model } => lines.push(Line::from(Span::styled(format!("▸ session {session} {}", model.clone().unwrap_or_default()), Style::default().fg(DIM)))),
            RunEvent::Text(t) => {
                for l in t.lines() {
                    lines.push(Line::from(l.to_string()));
                }
                lines.push(Line::from(""));
            }
            RunEvent::ToolUse { name, summary } => lines.push(Line::from(vec![Span::styled(format!("⚙ {name} "), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::styled(summary.clone(), Style::default().fg(DIM))])),
            RunEvent::ToolResult { summary, is_error } => lines.push(Line::from(Span::styled(format!("  ↳ {summary}"), Style::default().fg(if *is_error { ORANGE } else { DIM })))),
            RunEvent::Usage { input, output, cost_usd } => lines.push(Line::from(Span::styled(format!("Σ {input} in / {output} out{}", cost_usd.map(|c| format!(" · ${c:.3}")).unwrap_or_default()), Style::default().fg(DIM)))),
            RunEvent::Stderr(s) => lines.push(Line::from(Span::styled(format!("! {s}"), Style::default().fg(ORANGE)))),
            RunEvent::Raw(s) => lines.push(Line::from(Span::styled(s.clone(), Style::default().fg(DIM)))),
            RunEvent::Done { ok, summary } => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(if *ok { "✓ done" } else { "✗ failed" }, Style::default().fg(if *ok { Color::Green } else { ORANGE }).add_modifier(Modifier::BOLD))));
                for l in summary.lines() {
                    lines.push(Line::from(l.to_string()));
                }
            }
        }
    }
    if let RunStatus::Failed(why) = &run.meta.status {
        lines.push(Line::from(Span::styled(format!("✗ {why}"), Style::default().fg(ORANGE))));
    }
    let inner_h = area.height.saturating_sub(2) as usize;
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    // Estimate wrapped height so follow-mode pins to the bottom.
    let total: usize = lines.iter().map(|l| (l.width().max(1) + inner_w - 1) / inner_w).sum();
    let max_scroll = total.saturating_sub(inner_h) as u16;
    if ui.follow {
        ui.session_scroll = max_scroll;
    } else {
        ui.session_scroll = ui.session_scroll.min(max_scroll);
    }
    let block = pane_block(&title, ui.pane == Pane::Session).title_bottom(Span::styled(format!(" {log_path} "), Style::default().fg(DIM)));
    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((ui.session_scroll, 0)).block(block);
    f.render_widget(p, area);
}

fn draw_log(f: &mut Frame, st: &AppState, area: Rect) {
    let h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = st.log.iter().rev().take(h).rev().map(|(t, m)| Line::from(vec![Span::styled(format!("{} ", t.format("%H:%M:%S")), Style::default().fg(DIM)), Span::raw(m.clone())])).collect();
    f.render_widget(Paragraph::new(lines).block(pane_block("scheduler log", false)), area);
}

fn draw_footer(f: &mut Frame, ui: &Ui, area: Rect) {
    let keys = [("tab", "pane"), ("j/k", "move"), ("enter", "filter/open"), ("esc", "clear"), ("r", "run now"), ("R", "reload"), ("l", if ui.show_log { "session" } else { "log" }), ("g/G", "top/follow"), ("q", "quit")];
    let spans: Vec<Span> = keys.iter().flat_map(|(k, v)| [Span::styled(format!(" {k}"), Style::default().fg(GOLD)), Span::styled(format!(" {v} "), Style::default().fg(DIM))]).collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pane_block(title: &str, focused: bool) -> Block<'static> {
    let style = if focused { Style::default().fg(ACCENT) } else { Style::default().fg(DIM) };
    Block::default().borders(Borders::ALL).border_style(style).title(Span::styled(format!(" {title} "), style.add_modifier(Modifier::BOLD)))
}

/// `~`-relative path for display.
fn short_path(p: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    }
}

fn status_str(s: &RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed(_) => "failed",
    }
}

fn rel(d: chrono::Duration) -> String {
    if d.num_seconds() < 0 {
        return "now".into();
    }
    format!("in {}", rel_dur(d))
}

fn rel_past(d: chrono::Duration) -> String {
    format!("{} ago", rel_dur(d))
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
