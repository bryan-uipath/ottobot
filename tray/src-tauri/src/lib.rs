//! Menu-bar client for the ottobot daemon: tray icon + menu, popover board fed by `Snapshot`.

mod sock;

use anyhow::{bail, Context, Result};
use sock::{Conn, Request, Response, RunStatus, Snapshot};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, State, WindowEvent, Wry};

const TRAY: &str = "main";
const POPOVER: &str = "popover";
const POLL_EVERY: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Tray {
    conn: tokio::sync::Mutex<Option<Conn>>,
    /// Skill names in the `Run now` submenu.
    skills: Mutex<Vec<String>>,
    /// Skills root of the last daemon seen; `Open TUI` passes it to `otto`.
    root: Mutex<Option<PathBuf>>,
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
    tooltip(app, &format!("ottobot · {running} running · {failed} failed"));
    *t.root.lock().unwrap() = Some(st.root.clone());
    let names: Vec<String> = st.skills.iter().map(|v| v.name.clone()).collect();
    if *t.skills.lock().unwrap() != names {
        if let (Ok(menu), Some(tray)) = (build_menu(app, &names), app.tray_by_id(TRAY)) {
            let _ = tray.set_menu(Some(menu));
        }
        *t.skills.lock().unwrap() = names;
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
        Ok(s) => apply_snapshot(app, s),
        Err(e) if format!("{e:#}").contains(sock::OLD_PROTOCOL) => tooltip(app, &format!("ottobot · {}", sock::OLD_PROTOCOL)),
        Err(_) => tooltip(app, "ottobot · daemon not running"),
    }
    res
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
    win.set_position(PhysicalPosition::new(x, pos.y + size.height + 4.0 * scale))?;
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

/// Terminal running `otto <root>`, via a `.command` file so no AppleScript quoting is needed.
#[tauri::command]
fn open_tui(app: AppHandle) -> Result<(), String> {
    let _ = hide(&app);
    let root = app.state::<Tray>().root.lock().unwrap().clone();
    let cmd = match root {
        Some(r) => format!("#!/bin/sh\nexec otto '{}'\n", r.display().to_string().replace('\'', r"'\''")),
        None => "#!/bin/sh\nexec otto\n".into(),
    };
    let write = || -> std::io::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = dirs::home_dir().ok_or(std::io::ErrorKind::NotFound)?.join(".ottobot/open-tui.command");
        std::fs::write(&path, cmd)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    };
    let path = write().map_err(|e| e.to_string())?;
    std::process::Command::new("open").args(["-a", "Terminal"]).arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
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
        "open_tui" => return drop(open_tui(app)),
        "quit" => return app.exit(0),
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
        .manage(Tray::default())
        .invoke_handler(tauri::generate_handler![snapshot, run_now, open_tui, quit])
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
