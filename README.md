<p align="center"><img src="assets/otto-blue.svg" width="120" alt="Otto"></p>

# ottobot

A higher-order coding agent. Point `otto` at a folder of skill markdown files,
each with a `trigger:` frontmatter. It evaluates triggers on a tick, fires an
agent run (`claude -p` or `codex exec`) when one is due, and shows
everything in a terminal UI: what's running, recent failures, runner quota,
upcoming fires, run history, and live session output.

![otto TUI](docs/otto-tui.png)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bryan-uipath/ottobot/main/install.sh | sh
otto                          # watches ~/.ottobot/skills
```

Installs `otto` to `~/.local/bin` (macOS arm64/x86_64, Linux x86_64 static);
the one-liner needs the repo to be public.
`| sh -s -- --tray` also installs the menu-bar app to `~/Applications`;
`--uninstall` removes both (keeps `~/.ottobot`). Pin with `OTTO_VERSION=v0.2.0`,
relocate with `OTTO_INSTALL_DIR`, preview with `OTTO_DRY_RUN=1`.

From source: `cargo install --path .` (to `~/.cargo/bin`).

Requires `claude` and/or `codex` on `PATH`. macOS runs are kept awake via `caffeinate`.
`otto daemon --install` keeps the scheduler running without the TUI.

The tray app is ad-hoc signed, not notarized. `install.sh` strips the quarantine
flag; if you downloaded the zip yourself, allow it on first launch via System
Settings → Privacy & Security → Open Anyway, or run
`xattr -dr com.apple.quarantine "ottobot tray.app"`.

## A skill

`~/.ottobot/skills/playwright-ci-triage.md`:

```markdown
---
name: playwright-ci-triage
description: Triage a failed scheduled Playwright CI run
model: opus
trigger:
  event: "Scheduled run of https://github.com/UiPath/flow-workbench/actions/workflows/playwright-ci.yml failed"
  check_every: 5m
  cooldown: 8h
timeout: 45m
---

A scheduled Playwright CI run failed. Find the failing tests, decide flake vs
regression, and write a short triage summary.
```

| key | meaning | default |
|---|---|---|
| `name` | skill id, used in run ids and state | file stem |
| `runner` | `claude` or `codex` | `--runner` or config |
| `model` | `opus`, `codex/gpt-6-astra`, a config alias, or a list — see Routing | runner default |
| `checker_model` | model for agent-evaluated event triggers | sonnet (claude); runner default otherwise |
| `permission_mode` | claude `--permission-mode`; for codex, `bypassPermissions` → no sandbox, anything else → `-s <mode>` | `bypassPermissions` |
| `trigger.cron` | 5- or 6-field cron | |
| `trigger.event` | prose condition, compiled to a checker script (see below) | |
| `trigger.check_every` | event poll interval | 5m |
| `trigger.cooldown` | min gap before the same condition re-fires | 8h event, 0 cron |
| `timeout` | kill the run after this | 1h |

Exactly one of `cron` / `event`.

## Routing

`model` names a model on the skill's runner, a `runner/model` pair, or a list of
either. With a list, the run goes to the candidate whose runner has the most
quota left (tightest window; unknown usage counts as 50%); ties keep list order.
The pick is logged (`routed to codex/gpt-6-astra (97% left vs claude 55%)`).

```yaml
model: [codex/gpt-6-astra@medium, claude/opus@medium]   # whichever has more headroom
model: tier1                                             # alias from config.json
```

`@minimal|low|medium|high|xhigh|max` sets reasoning effort (`claude --effort`,
codex `model_reasoning_effort`); any other `@…` stays part of the model id.

Routing compares **runners**, not models: candidates on the same runner share
one quota, so list one model per runner. Unknown usage ranks below any known
runner; a runner that failed preflight is a last resort.

`~/.ottobot/config.json` can define aliases (nested up to 3 levels):

```json
{ "models": { "tier1": ["codex/gpt-6-astra@medium", "claude/opus@medium"], "tier3": ["claude/haiku"] } }
```

## Power

The daemon polls `pmset -g batt` every 30s. On battery the TUI strip and tray
header turn orange and the tray icon gets a badge: caffeinate only blocks idle
sleep, so a closed lid still interrupts runs. `otto status` includes the power
state.

## Reports

Every run gets `$OTTO_REPORT` = `<store>/reports/<run-id>.html`. A skill that
asks for a report should write a single self-contained HTML file there; if the
file exists when the run ends, the TUI shows `⎙ report` (`o` opens it) and the
tray shows a `report` link under the skill.

## Event triggers

On first check, `checker_model` writes a script for the condition to
`~/.ottobot/projects/<root>/checkers/<skill>-<trigger-hash>` and verifies it
once. Every `check_every` the script runs (cwd = skills root, `$OTTO_SKILL`
set) and prints one JSON object:

```json
{"fired": true, "key": "run-1234", "summary": "playwright-ci #1234 failed: 3 tests"}
```

`key` is the dedupe key (same key never fires twice); `summary` is passed to
the run as trigger context. If the script errors, the checker model answers
directly with tools; three consecutive script failures delete the script so
the next check recompiles. Editing the trigger changes the hash, so it also
recompiles. Hand-edit the script if the model got it wrong.

```
otto --check <skill>        compile if needed, run the checker once, print the verdict
```

## Dedupe

A due trigger is skipped when the skill already has an active run, the same
trigger key already fired, or the cooldown hasn't elapsed. The skip reason is
shown inline on the skill. `r` (run now) bypasses all three.

Two hashes per skill: the whole file, and the trigger block alone. Editing the
prompt body never resets dedupe state; editing the trigger does.

## CLI

```
otto [ROOT]                 TUI. ROOT defaults to config skills_root, then ~/.ottobot/skills
otto daemon [ROOT]          scheduler + socket server, foreground (see Daemon)
otto daemon --install       LaunchAgent: start at login, keep alive (--uninstall removes it)
otto status [--json]        one-line daemon summary; exit 1 if no daemon
otto --list                 discovered skills, hashes, parsed triggers
otto --run <skill>          one headless run, events to stdout (bypasses dedupe)
otto --show <run-id>        dump a stored run's metadata and events
otto --check <skill>        evaluate an event trigger once (compiles the checker if needed)
otto --usage                each runner's version and quota (claude 5h/7d, codex 7d)
otto --tick 30s --runner claude
```

Keys: `tab` cycle NOW / ATTENTION / NEXT / session · `j/k` move · `enter` open
session (on a NEXT skill: its history) · `esc` clear filter · `r` run now ·
`R` reload · `h` history · `l` scheduler log · `g/G` top / follow · `?` about ·
`q` quit.

## Daemon

`otto daemon` runs the scheduler, store, caffeinate, and usage refresh, and
serves `~/.ottobot/otto.sock` (mode 0600). One daemon per machine, one skills
root. On start it removes a stale socket, refuses (exit 1) if another daemon
answers, and writes `~/.ottobot/daemon.json` (`pid`, `root`, `socket`,
`started_at`); SIGINT/SIGTERM remove both.

`--install` writes `~/Library/LaunchAgents/com.ottobot.daemon.plist`
(`otto daemon <root>`, RunAtLoad, KeepAlive, your current `PATH` so
`claude`/`codex` resolve, stdout/stderr to `daemon.{out,err}.log` in the store
dir) and `launchctl bootstrap`s it. `--uninstall` boots it out and deletes the
plist. macOS only.

`otto` connects to the socket first: same root → TUI drives the daemon
(status strip shows its pid, or "daemon disconnected · retrying" every 2s); different root
→ prints both and exits 1; no daemon → in-process scheduler as before (exits
with the TUI). `--run` goes through the daemon when one is up; `--check` is
always in-process. Locks are `flock`s the kernel drops on exit:
`~/.ottobot/daemon.lock` (one daemon per machine) and `<store>/scheduler.pid`
(one scheduler per root, daemon or in-process). `--install` forwards explicit
`--tick`/`--runner`.

The protocol is unversioned: after upgrading the binary, restart the daemon
(a client that can't parse its answers exits with that hint).

Wire: newline-delimited JSON, one request → one response, connection reusable.

```
{"type":"Snapshot"}                         → {"type":"Snapshot","data":{root, skills, runs (meta only), log, …}}
{"type":"Events","run":"<id>","from":N}     → {"type":"Events","data":[…events N..]}
{"type":"RunNow","skill":"<name>"}          → {"type":"Ok"} | {"type":"Error","data":"no skill …"}
{"type":"Reload"}                           → {"type":"Ok"}
{"type":"Status"}                           → {"type":"Status","data":{root, pid, running, skills, last_failure, next_fire, runner_usage}}
```

## Tray

`tray/` is a macOS menu-bar app (Tauri v2): tooltip with running/failed counts,
a popover with the same NOW / ATTENTION / QUOTA / NEXT board, and Run now /
Reload from its menu. It is a socket client only, so it needs the
daemon. Build: `cd tray/src-tauri && cargo tauri build`. See `tray/README.md`.

## Runners and usage

`runner/` implements `Runner` per agent CLI: `command()`, `parse_line()` into
normalized `RunEvent`s, `preflight()`, and `usage()`. Usage reports each
rate-limit window as a percent used so the scheduler can route to whichever
runner has headroom (not wired yet; TUI QUOTA section refreshes it every 5m, `--usage` prints it).

| runner | invocation | usage source |
|---|---|---|
| `claude` | `claude -p --output-format stream-json` | `api.anthropic.com/api/oauth/usage` with the Claude Code OAuth token (keychain or `~/.claude/.credentials.json`) |
| `codex` | `codex exec --json -C <cwd>` | last `rate_limits` in the newest `~/.codex/sessions/**/*.jsonl` rollout |

## Config and state

`~/.ottobot/config.json` (all optional, CLI flags win):

```json
{ "skills_root": "~/code/my-skills", "runner": "claude", "tick": "1m" }
```

Per-project state lives in `~/.ottobot/projects/<root-path-with-dashes>/`,
same scheme as `~/.claude/projects`:

```
state.json            per-skill: hashes, last fired key/time, last run id, last check
checkers/<skill>-<h>  compiled event checker scripts
runs/<id>.json        run metadata (status, cost, tokens, cwd)
runs/<id>.jsonl       normalized event stream (text, tool calls, results, usage)
ottobot.log.<date>    scheduler log
```

Runs left `Running` by a crash are reported as failed on next start.

## `/otto` Claude Code skill

`claude-skill/otto/SKILL.md` teaches Claude Code to distill the current
conversation into an ottobot skill: after a one-off investigation, `/otto`
checks whether there is a clear trigger, a repeatable procedure, and a
definition of done, then writes the skill file into your skills root.

```sh
ln -s "$PWD/claude-skill/otto" ~/.claude/skills/otto
```

## Architecture

```
skill.rs      discover *.md with trigger frontmatter, hash file + trigger block
trigger.rs    cron via croner; event spec parsing
checker.rs    event checks: compile prose → script, run it, agent fallback
scheduler.rs  tick loop, dedupe gate, spawn + stream runs
route.rs      model candidates + config aliases → runner/model with the most quota left
runner/       Runner trait: command(), parse_line() -> RunEvent, preflight(), usage(); claude + codex
store.rs      ~/.ottobot/projects/<root>/ state, run metadata, event logs
state.rs      shared in-memory state between scheduler and clients
proto.rs      Snapshot/Status DTOs, socket Request/Response, request handler
daemon.rs     `otto daemon`: socket server, daemon.json, signals, launchd install
client.rs     Backend: in-process (Local) or socket (Remote, reconnecting)
tui/          ratatui over a Backend: status strip, board rail (now/attention/quota/next) or history, session/log, `?` about
otto.rs       Otto art (about overlay) from assets/otto.ans
tray/         macOS menu-bar client (Tauri v2, static HTML popover) over the daemon socket
```

## Roadmap

1. ✅ discovery, hashing, cron triggers, claude runner, TUI, caffeinate, config
2. ✅ event triggers: compile prose → checker script via sonnet, agent fallback, dedupe key from checker output
3. `env.repo`: fetch, worktree per run on `ottobot/<skill>/<run-id>`, worktree removed after, branch kept
4. ✅ codex runner, per-runner preflight + usage
4b. ✅ route by usage: `model: [a, b]` picks the runner with the most quota left
5. ✅ headless daemon + `otto status`
6. ✅ macOS menu-bar tray (daemon client)

## Otto art

`assets/otto.ans` is generated from the flat SVG:

```sh
RAMP=" █" cargo run -p otto-render -- assets/otto-blue.png 40 0 6 > assets/otto.ans
```

Args: source, columns, luminance weight, alpha-opening radius (strips the
propeller motion-blur arcs). A symmetric blade is drawn over the detected hub.
