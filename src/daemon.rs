//! `otto daemon`: scheduler + store + caffeinate behind a Unix socket, plus launchd install.

use crate::client::{socket_path, Conn};
use crate::proto::{self, Request, Response};
use crate::scheduler::Command;
use crate::state::Shared;
use crate::store::{FileLock, Store};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const LABEL: &str = "com.ottobot.daemon";

fn info_path() -> Result<PathBuf> {
    Ok(crate::config::home()?.join("daemon.json"))
}

/// Run in the foreground until SIGINT/SIGTERM. Lock order: daemon.lock → scheduler.pid → socket,
/// so a second daemon can never unlink a live one's socket or double-schedule.
pub async fn serve(
    root: PathBuf,
    store: Store,
    tick: Duration,
    default_runner: String,
    aliases: std::collections::HashMap<String, Vec<String>>,
) -> Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let home = crate::config::home()?;
    // Socket and lock live here; owner-only so no other local user can connect.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }
    let sock = socket_path()?;
    let _daemon_lock = match FileLock::acquire(&home.join("daemon.lock")) {
        Ok(l) => l,
        Err(e) => {
            match Conn::probe(&sock).await {
                Ok((_, s)) => eprintln!("otto daemon already running: pid {} · {}", s.pid, s.root.display()),
                Err(_) => eprintln!("otto daemon already starting: {e:#}"),
            }
            std::process::exit(1);
        }
    };
    let sched_lock = store.lock_scheduler()?;
    // We hold the daemon lock, so whatever is at the socket path is a leftover.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let core = crate::start_core(root.clone(), store, tick, default_runner, aliases, sched_lock);
    {
        let mut st = core.shared.lock().unwrap();
        st.daemon_pid = Some(std::process::id());
        st.log(format!("daemon listening on {}", sock.display()));
    }
    let info = serde_json::json!({ "pid": std::process::id(), "root": root, "socket": sock, "started_at": chrono::Local::now(), "commit": proto::COMMIT });
    std::fs::write(info_path()?, serde_json::to_string_pretty(&info)?)?;
    println!("otto daemon pid {} · {} · {}", std::process::id(), root.display(), sock.display());

    let res = loop {
        tokio::select! {
            a = listener.accept() => match a {
                Ok((stream, _)) => { tokio::spawn(serve_conn(stream, core.shared.clone(), core.cmds.clone())); }
                Err(e) => break Err(anyhow::Error::from(e).context("accept")),
            },
            _ = term.recv() => break Ok(()),
            _ = int.recv() => break Ok(()),
        }
    };
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(info_path()?);
    drop(core); // kills caffeinate, releases scheduler lock
    tracing::info!("daemon stopped");
    res
}

/// One client: request lines in, response lines out, until EOF.
async fn serve_conn(stream: UnixStream, shared: Shared, cmds: mpsc::Sender<Command>) {
    // Second layer behind the 0600 socket: only our own uid may trigger runs.
    match stream.peer_cred() {
        Ok(c) if c.uid() == unsafe { libc::getuid() } => {}
        _ => return,
    }
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => proto::handle(&shared, &cmds, req).await,
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let Ok(mut out) = serde_json::to_string(&resp) else { break };
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

fn plist_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home dir")?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// `$UID` if exported, else `id -u`.
fn uid() -> Result<String> {
    if let Ok(u) = std::env::var("UID") {
        return Ok(u);
    }
    let out = std::process::Command::new("id").arg("-u").output().context("id -u")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn require_macos(verb: &str) {
    if !cfg!(target_os = "macos") {
        eprintln!("{verb} is macOS-only");
        std::process::exit(1);
    }
}

fn target(uid: &str) -> String {
    format!("gui/{uid}/{LABEL}")
}

fn launchctl(args: &[&str]) -> Result<()> {
    let st = std::process::Command::new("launchctl").args(args).status().context("launchctl")?;
    if !st.success() {
        bail!("launchctl {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// Write the LaunchAgent plist and bootstrap it. Logs go to the store dir.
pub fn install(root: &Path, store_dir: &Path, extra_args: &[String]) -> Result<()> {
    require_macos("install");
    let exe = std::env::current_exe()?.canonicalize()?;
    let path = std::env::var("PATH").unwrap_or_default();
    let esc = crate::store::html_escape;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
    <string>{root}</string>{extra}
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>{path}</string>
  </dict>
</dict>
</plist>
"#,
        exe = esc(&exe.to_string_lossy()),
        root = esc(&root.to_string_lossy()),
        extra = extra_args.iter().map(|a| format!("\n    <string>{}</string>", esc(a))).collect::<String>(),
        out = esc(&store_dir.join("daemon.out.log").to_string_lossy()),
        err = esc(&store_dir.join("daemon.err.log").to_string_lossy()),
        path = esc(&path),
    );
    let p = plist_path()?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let uid = uid()?;
    // Re-install: unload the old definition first; failure just means it wasn't loaded.
    if p.exists() {
        let _ = launchctl(&["bootout", &target(&uid)]);
    }
    std::fs::write(&p, plist)?;
    println!("wrote {}", p.display());
    launchctl(&["bootstrap", &format!("gui/{uid}"), &p.to_string_lossy()])?;
    println!("loaded {LABEL} (gui/{uid}) · root {}", root.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_macos("uninstall");
    let uid = uid()?;
    match launchctl(&["bootout", &target(&uid)]) {
        Ok(()) => println!("unloaded {LABEL} (gui/{uid})"),
        Err(e) => eprintln!("{e:#} (not loaded?)"),
    }
    let p = plist_path()?;
    if p.exists() {
        std::fs::remove_file(&p)?;
        println!("removed {}", p.display());
    }
    Ok(())
}
