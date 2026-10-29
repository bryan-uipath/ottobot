# Changelog

## 0.2.0

- Codex runner (`codex exec --json`) and per-runner quota via `otto --usage`.
- `otto daemon`: headless scheduler on `~/.ottobot/otto.sock`, `--install` as a launchd LaunchAgent, `otto status`.
- Board TUI: NOW / ATTENTION / QUOTA / NEXT, latest-run line per skill.
- macOS tray app (Tauri v2) over the daemon socket.
- Model routing by remaining quota (`model: [a, b]`, config aliases), `@effort`, `$OTTO_REPORT` HTML reports.
- `/otto` Claude Code skill: distill a conversation into an ottobot skill.
- Prebuilt releases and `install.sh`.

## 0.1.0

- Skill discovery, cron and event triggers, claude runner, TUI, caffeinate.
