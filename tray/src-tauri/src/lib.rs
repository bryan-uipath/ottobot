//! Menu-bar client for the ottobot daemon: tray icon + menu, popover board fed by `Snapshot`.

mod sock;

use anyhow::{bail, Context, Result};
use sock::{Conn, Level, Request, Response, RunStatus, Snapshot};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, State, WindowEvent, Wry};
use tauri_plugin_notification::NotificationExt;

const TRAY: &str = "main";
const POPOVER: &str = "popover";
const POLL_EVERY: Duration = Duration::from_secs(15);
/// Short SHA this tray was built from; see build.rs.
const COMMIT: &str = env!("OTTO_COMMIT");

/// Skill names + latest report per skill: what the tray menu is built from.
type MenuKey = (Vec<String>, Vec<(String, PathBuf)>);

/// Tray icon, highest precedence first: alerts outrank the battery badge.
#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Immediate,
    Critical,
    Battery,
    Plain,
}

#[derive(Default)]
struct Tray {
    conn: tokio::sync::Mutex<Option<Conn>>,
    /// Skill names in the `Run now` submenu.
    skills: Mutex<Vec<String>>,
    /// What the menu was last built from (skills + latest reports), to rebuild only on change.
    menu_key: Mutex<Option<MenuKey>>,
    /// Store dir of the daemon; `Open reports folder` uses `<store>/reports`.
    store_dir: Mutex<Option<PathBuf>>,
    /// Skills root of the last daemon seen; `Skills` opens it in Finder.
    root: Mutex<Option<std::path::PathBuf>>,
    /// Which icon is showing, to avoid resetting it every poll.
    icon: Mutex<Option<Icon>>,
    /// When blur hid the popover; a tray click right after must not reopen it.
    hidden_at: Mutex<Option<Instant>>,
}

impl Tray {
    /// One request on the shared connection; reconnects lazily, drops it on any I/O error.
    async fn request(&self, req: Request) -> Result<Response> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(Conn::connect(&sock::socket_path()?).await.context("daemon not running")?);
        }
        let Some(conn) = guard.as_mut() else { bail!("daemon not running") };
        let res = conn.request(&req).await;
        if res.is_err() {
            *guard = None;
        }
        match res? {
            Response::Error(e) => bail!(e),
            r => Ok(r),
        }
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        match self.request(Request::Snapshot).await? {
            Response::Snapshot(s) => Ok(s),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

/// Tooltip, root, and `Run now` submenu from a fresh snapshot.
fn apply_snapshot(app: &AppHandle, st: &Snapshot) {
    let t = app.state::<Tray>();
    let since = chrono::Local::now() - chrono::Duration::hours(24);
    let failed = st.runs.iter().filter(|r| matches!(r.status, RunStatus::Failed(_)) && r.started_at >= since).count();
    let running = st.runs.iter().filter(|r| r.status == RunStatus::Running).count();
    let batt = st.power.as_ref().filter(|p| !p.on_ac);
    let warn = batt.map(|p| format!(" · ⚠ {}", p.summary())).unwrap_or_default();
    let n = st.alerts.len();
    let alerts = if n == 0 { String::new() } else { format!("{n} alert{} · ", if n == 1 { "" } else { "s" }) };
    // Protocol is unversioned: flag a tray/daemon build mismatch.
    let (differ, daemon) = (st.commit != COMMIT, if st.commit.is_empty() { "?" } else { &st.commit });
    let ver = format!("{}tray {COMMIT} · daemon {daemon}", if differ { "⚠ " } else { "" });
    tooltip(app, &format!("{alerts}ottobot · {running} running · {failed} failed{warn}\n{ver}"));
    set_icon(
        app,
        match st.alerts.iter().map(|a| a.level).max() {
            Some(Level::Immediate) => Icon::Immediate,
            Some(Level::Critical) => Icon::Critical,
            _ if batt.is_some() => Icon::Battery,
            _ => Icon::Plain,
        },
    );
    let names: Vec<String> = st.skills.iter().map(|v| v.name.clone()).collect();
    // Latest report per skill; runs are newest first.
    let mut seen = std::collections::HashSet::new();
    let reports: Vec<(String, PathBuf)> =
        st.runs.iter().filter_map(|r| r.report.clone().map(|p| (r.skill.clone(), p))).filter(|(s, _)| seen.insert(s.clone())).collect();
    let reports: Vec<(String, PathBuf)> = reports.into_iter().filter(|(_, p)| p.is_file()).collect();
    *t.root.lock().unwrap() = Some(st.root.clone());
    *t.store_dir.lock().unwrap() = Some(st.store_dir.clone());
    let key = (names.clone(), reports.clone());
    if *t.menu_key.lock().unwrap() != Some(key.clone()) {
        if let (Ok(menu), Some(tray)) = (build_menu(app, &names, &reports), app.tray_by_id(TRAY)) {
            let _ = tray.set_menu(Some(menu));
        }
        *t.menu_key.lock().unwrap() = Some(key);
        *t.skills.lock().unwrap() = names;
    }
}

/// Yellow/red dot for the worst active alert; else badged template icon while on battery (runs may be cut
/// short by sleep); plain otherwise. Coloured icons can't be templates: their glyph carries a white halo instead.
fn set_icon(app: &AppHandle, icon: Icon) {
    let t = app.state::<Tray>();
    if *t.icon.lock().unwrap() == Some(icon) {
        return;
    }
    let bytes: &[u8] = match icon {
        Icon::Immediate => include_bytes!("../icons/tray-immediate.png"),
        Icon::Critical => include_bytes!("../icons/tray-critical.png"),
        Icon::Battery => include_bytes!("../icons/tray-warn.png"),
        Icon::Plain => include_bytes!("../icons/tray.png"),
    };
    if let (Ok(img), Some(tray)) = (tauri::image::Image::from_bytes(bytes), app.tray_by_id(TRAY)) {
        let _ = tray.set_icon(Some(img));
        let _ = tray.set_icon_as_template(matches!(icon, Icon::Battery | Icon::Plain));
        *t.icon.lock().unwrap() = Some(icon);
    }
}

fn tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY) {
        let _ = tray.set_tooltip(Some(text));
    }
}

/// Fetch a snapshot and apply it, or show why not.
async fn refresh(app: &AppHandle) -> Result<Snapshot> {
    let res = app.state::<Tray>().snapshot().await;
    match &res {
        Ok(s) => {
            apply_snapshot(app, s);
            notify(app, s).await;
        }
        Err(e) => {
            // No daemon → no power or alert data; don't leave a stale badge up.
            set_icon(app, Icon::Plain);
            let old = format!("{e:#}").contains(sock::OLD_PROTOCOL);
            tooltip(app, &format!("ottobot · {}\ntray {COMMIT}", if old { sock::OLD_PROTOCOL } else { "daemon not running" }));
        }
    }
    res
}

/// Post the daemon's pending immediate-alert notifications. Claim first, so overlapping refreshes and
/// tray restarts post each once. Text is untrusted LLM output: plain strings to the API, no shell.
async fn notify(app: &AppHandle, st: &Snapshot) {
    for a in st.alerts.iter().filter(|a| a.notify) {
        let claim = Request::Notified { skill: a.skill.clone(), key: a.key.clone() };
        if app.state::<Tray>().request(claim).await.is_err() {
            continue;
        }
        let mut n = app.notification().builder().title(format!("otto · {} · {}", a.skill, a.headline)).body(first_sentence(&a.summary));
        if let Some(p) = &a.report {
            n = n.extra("report", p);
        }
        let _ = n.show();
    }
}

/// "Disk full on db-1. Writes failing." → "Disk full on db-1."
fn first_sentence(s: &str) -> &str {
    let s = s.trim();
    let end = s.char_indices().find(|&(i, c)| matches!(c, '.' | '!' | '?') && s[i + 1..].starts_with(char::is_whitespace));
    s[..end.map_or(s.len(), |(i, _)| i + 1)].lines().next().unwrap_or("")
}

/// `reports`: (skill, report path) for the latest report per skill.
fn build_menu(app: &AppHandle, skills: &[String], reports: &[(String, PathBuf)]) -> tauri::Result<Menu<Wry>> {
    let runs =
        skills.iter().map(|s| MenuItem::with_id(app, format!("run:{s}"), s, true, None::<&str>)).collect::<tauri::Result<Vec<_>>>()?;
    let refs: Vec<&dyn IsMenuItem<Wry>> = runs.iter().map(|m| m as &dyn IsMenuItem<Wry>).collect();
    let mut reps = reports
        .iter()
        .map(|(s, p)| MenuItem::with_id(app, format!("report:{}", p.display()), s, true, None::<&str>))
        .collect::<tauri::Result<Vec<_>>>()?;
    reps.push(MenuItem::with_id(app, "reports_dir", "Open reports folder", true, None::<&str>)?);
    let rrefs: Vec<&dyn IsMenuItem<Wry>> = reps.iter().map(|m| m as &dyn IsMenuItem<Wry>).collect();
    Menu::with_items(
        app,
        &[
            &Submenu::with_id_and_items(app, "run_now", "Run now", !skills.is_empty(), &refs)?,
            &Submenu::with_id_and_items(app, "reports", "Reports", true, &rrefs)?,
            &MenuItem::with_id(app, "reload", "Reload skills", true, None::<&str>)?,
            &MenuItem::with_id(app, "skills", "Open skills folder", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )
}

/// Show the popover centered under the tray icon (clamped to that monitor's work area), or hide it.
fn toggle(app: &AppHandle, rect: Rect) -> tauri::Result<()> {
    let Some(win) = app.get_webview_window(POPOVER) else { return Ok(()) };
    if win.is_visible()? {
        return hide(app);
    }
    if app.state::<Tray>().hidden_at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return Ok(());
    }
    // Monitor containing the icon, judged in that monitor's own scale.
    let on = |m: &tauri::Monitor| {
        let p = rect.position.to_physical::<f64>(m.scale_factor());
        let (mp, ms) = (m.position(), m.size());
        p.x >= mp.x as f64 && p.x < mp.x as f64 + ms.width as f64 && p.y >= mp.y as f64 && p.y < mp.y as f64 + ms.height as f64
    };
    let mon = app.available_monitors()?.into_iter().find(on).or(app.primary_monitor()?);
    let scale = mon.as_ref().map_or(win.scale_factor()?, |m| m.scale_factor());
    let pos = rect.position.to_physical::<f64>(scale);
    let size = rect.size.to_physical::<f64>(scale);
    let w = win.outer_size()?.to_logical::<f64>(win.scale_factor()?).width * scale;
    let mut x = pos.x + size.width / 2.0 - w / 2.0;
    if let Some(m) = &mon {
        let wa = m.work_area();
        x = x.min(wa.position.x as f64 + wa.size.width as f64 - w).max(wa.position.x as f64);
    }
    win.set_position(PhysicalPosition::new(x, pos.y + size.height - 2.0 * scale))?;
    // Caret x in CSS px, so it keeps pointing at the icon when the window is clamped at a screen edge.
    let caret = (pos.x + size.width / 2.0 - x) / scale;
    let _ = win.eval(format!("document.documentElement.style.setProperty('--caret-x','{caret:.0}px')"));
    win.show()?;
    win.set_focus()?;
    win.emit("visible", true)
}

fn hide(app: &AppHandle) -> tauri::Result<()> {
    let Some(win) = app.get_webview_window(POPOVER) else { return Ok(()) };
    win.hide()?;
    *app.state::<Tray>().hidden_at.lock().unwrap() = Some(Instant::now());
    win.emit("visible", false)
}

#[tauri::command]
async fn snapshot(app: AppHandle) -> Result<Snapshot, String> {
    refresh(&app).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn run_now(t: State<'_, Tray>, skill: String) -> Result<(), String> {
    t.request(Request::RunNow { skill }).await.map(|_| ()).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn dismiss(t: State<'_, Tray>, skill: String, key: String) -> Result<(), String> {
    t.request(Request::Dismiss { skill, key }).await.map(|_| ()).map_err(|e| format!("{e:#}"))
}

/// Skills folder in Finder. Falls back to ~/.ottobot/skills when no daemon has been seen.
#[tauri::command]
fn open_skills(t: State<'_, Tray>) -> Result<(), String> {
    let root = t.root.lock().unwrap().clone().or_else(|| dirs::home_dir().map(|h| h.join(".ottobot/skills"))).ok_or("no skills root")?;
    open_path(&root)
}

/// `open <path>` with the default handler, detached.
fn open_path(p: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("open").arg(p).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Open a run's HTML report. Only files under ~/.ottobot, so the webview can't open arbitrary paths.
#[tauri::command]
fn open_report(path: std::path::PathBuf) -> Result<(), String> {
    // Canonicalize both sides: ~/.ottobot may itself be a symlink (dotfiles).
    let home = dirs::home_dir().ok_or("no home")?.join(".ottobot").canonicalize().map_err(|e| e.to_string())?;
    let real = path.canonicalize().map_err(|e| e.to_string())?;
    if !real.starts_with(&home) || !matches!(real.extension().and_then(|e| e.to_str()), Some("html" | "md")) {
        return Err("not an ottobot report".into());
    }
    open_path(&real)
}

#[tauri::command]
fn commit() -> &'static str {
    COMMIT
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// While hidden: a snapshot every 15s for tooltip + menu (the popover polls its own while open).
async fn poll(app: AppHandle) {
    loop {
        if !app.get_webview_window(POPOVER).is_some_and(|w| w.is_visible().unwrap_or(false)) {
            let _ = refresh(&app).await;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// Menu action, then an immediate snapshot; an error stays in the tooltip until the next poll.
async fn menu_action(app: AppHandle, id: String) {
    let req = match id.as_str() {
        "quit" => return app.exit(0),
        "skills" => return drop(open_skills(app.state::<Tray>())),
        "reports_dir" => {
            if let Some(d) = app.state::<Tray>().store_dir.lock().unwrap().clone() {
                let _ = open_path(&d.join("reports"));
            }
            return;
        }
        id if id.starts_with("report:") => return drop(open_report(PathBuf::from(&id["report:".len()..]))),
        "reload" => Request::Reload,
        id => match id.strip_prefix("run:") {
            Some(skill) => Request::RunNow { skill: skill.to_string() },
            None => return,
        },
    };
    match app.state::<Tray>().request(req).await {
        Ok(_) => drop(refresh(&app).await),
        Err(e) => tooltip(&app, &format!("ottobot · {e:#}")),
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(Tray::default())
        .invoke_handler(tauri::generate_handler![snapshot, run_now, dismiss, open_skills, open_report, commit, quit])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            // Click on a notification opens its report (same path checks as the popover).
            app.notification().on_action(|e| {
                let report = e.notification().and_then(|n| n.extra().get("report")).and_then(|v| v.as_str());
                if let (Some(p), "tap") = (report, e.action_id()) {
                    let _ = open_report(PathBuf::from(p));
                }
            })?;
            TrayIconBuilder::with_id(TRAY)
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("ottobot")
                .menu(&build_menu(&handle, &[], &[])?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| drop(tauri::async_runtime::spawn(menu_action(app.clone(), ev.id().as_ref().to_string()))))
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = ev {
                        let _ = toggle(tray.app_handle(), rect);
                    }
                })
                .build(app)?;
            tauri::async_runtime::spawn(poll(handle));
            Ok(())
        })
        .on_window_event(|win, ev| {
            if let WindowEvent::Focused(false) = ev {
                let _ = hide(win.app_handle());
            }
        })
        .run(tauri::generate_context!())
        .expect("tauri app");
}

#[cfg(test)]
mod tests {
    #[test]
    fn first_sentence() {
        assert_eq!(super::first_sentence(" v1.2 down! 0 rps since 10:00. Rollback?"), "v1.2 down!");
        assert_eq!(super::first_sentence("no stop\nsecond line"), "no stop");
        assert_eq!(super::first_sentence(""), "");
    }
}
