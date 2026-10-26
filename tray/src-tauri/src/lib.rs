//! Menu-bar client for the ottobot daemon: tray icon + menu, popover board fed by `Snapshot`.

mod sock;

use anyhow::{bail, Result};
use sock::{Conn, Request, Response, RunStatus, Snapshot, Status};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, State, WindowEvent, Wry};

const TRAY: &str = "main";
const POPOVER: &str = "popover";
const STATUS_EVERY: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Tray {
    conn: tokio::sync::Mutex<Option<Conn>>,
    /// Skill names in the `Run now` submenu.
    skills: Mutex<Vec<String>>,
    /// Failed runs in the last 24h, from the latest snapshot (Status has no count).
    failed: Mutex<usize>,
    /// Newest failure seen; a change in `Status` triggers a snapshot refresh.
    last_failure: Mutex<Option<(String, String)>>,
    /// When blur hid the popover; a tray click right after must not reopen it.
    hidden_at: Mutex<Option<Instant>>,
}

impl Tray {
    /// One request on the shared connection; reconnects lazily, drops it on any I/O error.
    async fn request(&self, req: Request) -> Result<Response> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(Conn::connect(&sock::socket_path()?).await?);
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

    async fn status(&self) -> Result<Status> {
        match self.request(Request::Status).await? {
            Response::Status(s) => Ok(s),
            other => bail!("unexpected response: {other:?}"),
        }
    }
}

/// Tooltip and `Run now` submenu from a fresh snapshot.
fn apply_snapshot(app: &AppHandle, st: &Snapshot) {
    let t = app.state::<Tray>();
    let since = chrono::Local::now() - chrono::Duration::hours(24);
    let failed = st.runs.iter().filter(|r| matches!(r.status, RunStatus::Failed(_)) && r.started_at >= since).count();
    let running = st.runs.iter().filter(|r| r.status == RunStatus::Running).count();
    *t.failed.lock().unwrap() = failed;
    *t.last_failure.lock().unwrap() = st.runs.iter().find(|r| matches!(r.status, RunStatus::Failed(_))).map(|r| (r.skill.clone(), r.id.clone()));
    tooltip(app, Some((running, failed)));
    let names: Vec<String> = st.skills.iter().map(|v| v.name.clone()).collect();
    if *t.skills.lock().unwrap() != names {
        if let Ok(menu) = build_menu(app, &names) {
            if let Some(tray) = app.tray_by_id(TRAY) {
                let _ = tray.set_menu(Some(menu));
            }
        }
        *t.skills.lock().unwrap() = names;
    }
}

/// `ottobot · 2 running · 1 failed`, or the daemon-down hint.
fn tooltip(app: &AppHandle, counts: Option<(usize, usize)>) {
    let text = match counts {
        Some((r, f)) => format!("ottobot · {r} running · {f} failed"),
        None => "ottobot · daemon not running".into(),
    };
    if let Some(tray) = app.tray_by_id(TRAY) {
        let _ = tray.set_tooltip(Some(text));
    }
}

fn build_menu(app: &AppHandle, skills: &[String]) -> tauri::Result<Menu<Wry>> {
    let runs = skills.iter().map(|s| MenuItem::with_id(app, format!("run:{s}"), s, true, None::<&str>)).collect::<tauri::Result<Vec<_>>>()?;
    let refs: Vec<&dyn IsMenuItem<Wry>> = runs.iter().map(|m| m as &dyn IsMenuItem<Wry>).collect();
    Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "open_tui", "Open TUI", true, None::<&str>)?,
            &Submenu::with_id_and_items(app, "run_now", "Run now", !skills.is_empty(), &refs)?,
            &MenuItem::with_id(app, "reload", "Reload skills", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )
}

/// Show the popover centered under the tray icon, or hide it.
fn toggle(app: &AppHandle, rect: Rect) -> tauri::Result<()> {
    let Some(win) = app.get_webview_window(POPOVER) else { return Ok(()) };
    if win.is_visible()? {
        return hide(app);
    }
    if app.state::<Tray>().hidden_at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return Ok(());
    }
    let scale = win.scale_factor()?;
    let pos = rect.position.to_physical::<f64>(scale);
    let size = rect.size.to_physical::<f64>(scale);
    let w = win.outer_size()?.width as f64;
    win.set_position(PhysicalPosition::new(pos.x + size.width / 2.0 - w / 2.0, pos.y + size.height + 4.0 * scale))?;
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
async fn snapshot(app: AppHandle, t: State<'_, Tray>) -> Result<Snapshot, String> {
    match t.snapshot().await {
        Ok(s) => {
            apply_snapshot(&app, &s);
            Ok(s)
        }
        Err(e) => {
            tooltip(&app, None);
            Err(format!("{e:#}"))
        }
    }
}

#[tauri::command]
async fn status(t: State<'_, Tray>) -> Result<Status, String> {
    t.status().await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn run_now(t: State<'_, Tray>, skill: String) -> Result<(), String> {
    t.request(Request::RunNow { skill }).await.map(|_| ()).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn reload(t: State<'_, Tray>) -> Result<(), String> {
    t.request(Request::Reload).await.map(|_| ()).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn open_tui(app: AppHandle) -> Result<(), String> {
    let _ = hide(&app);
    std::process::Command::new("osascript")
        .args(["-e", r#"tell application "Terminal" to do script "otto""#, "-e", r#"tell application "Terminal" to activate"#])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// While hidden: `Status` every 15s for the tooltip; a full snapshot only when skills or the newest failure change.
async fn poll_status(app: AppHandle) {
    loop {
        let hidden = app.get_webview_window(POPOVER).map(|w| !w.is_visible().unwrap_or(false)).unwrap_or(true);
        if hidden {
            let t = app.state::<Tray>();
            match t.status().await {
                Ok(s) => {
                    let stale = s.skills != t.skills.lock().unwrap().len() || s.last_failure != *t.last_failure.lock().unwrap();
                    if !stale {
                        tooltip(&app, Some((s.running, *t.failed.lock().unwrap())));
                    } else if let Ok(snap) = t.snapshot().await {
                        apply_snapshot(&app, &snap);
                    } else {
                        tooltip(&app, None);
                    }
                }
                Err(_) => tooltip(&app, None),
            }
        }
        tokio::time::sleep(STATUS_EVERY).await;
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(Tray::default())
        .invoke_handler(tauri::generate_handler![snapshot, status, run_now, reload, open_tui, quit])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            TrayIconBuilder::with_id(TRAY)
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("ottobot")
                .menu(&build_menu(&handle, &[])?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| {
                    let id = ev.id().as_ref().to_string();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let t = app.state::<Tray>();
                        match id.as_str() {
                            "open_tui" => drop(open_tui(app.clone())),
                            "reload" => drop(t.request(Request::Reload).await),
                            "quit" => app.exit(0),
                            id => {
                                if let Some(skill) = id.strip_prefix("run:") {
                                    let _ = t.request(Request::RunNow { skill: skill.to_string() }).await;
                                }
                            }
                        }
                    });
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = ev {
                        let _ = toggle(tray.app_handle(), rect);
                    }
                })
                .build(app)?;
            tauri::async_runtime::spawn(poll_status(handle));
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
