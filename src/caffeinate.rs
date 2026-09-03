//! Keep the machine awake while ottobot runs (macOS `caffeinate -w <pid>` exits with us).

use std::process::{Child, Command};

pub fn start() -> Option<Child> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    Command::new("caffeinate").args(["-i", "-w", &std::process::id().to_string()]).spawn().ok()
}
