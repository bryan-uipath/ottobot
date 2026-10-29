//! Sets `OTTO_COMMIT`: short git SHA (+`-dirty`), or `unknown` without git. Also used by tray/src-tauri/build.rs.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn main() {
    let commit = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(sha) => {
            // Rerun on new commits (HEAD, its ref) and source edits (dirty); --git-path resolves worktrees.
            let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
            let watch = ["HEAD", head_ref.as_deref().unwrap_or("HEAD"), "packed-refs"];
            for p in watch.iter().filter_map(|p| git(&["rev-parse", "--git-path", p])) {
                // A missing path would rerun every build.
                if std::path::Path::new(&p).exists() {
                    println!("cargo:rerun-if-changed={p}");
                }
            }
            println!("cargo:rerun-if-changed=src");
            // Tracked files only: untracked scratch (e.g. .claude/worktrees) isn't a code change.
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            format!("{sha}{}", if dirty { "-dirty" } else { "" })
        }
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=OTTO_COMMIT={commit}");
}
