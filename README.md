<p align="center"><img src="assets/otto-blue.svg" width="120" alt="Otto"></p>

# ottobot

A higher-order coding agent. Point `otto` at a folder of skill markdown files,
each with a `trigger:` frontmatter. It evaluates triggers on a tick, fires an
agent run (`claude -p` today; codex/opencode next) when one is due, and shows
everything in a terminal UI: skills, upcoming fires, run history, and live
session output.

![otto TUI](docs/otto-tui.png)

## Install

```sh
cargo install --path .        # installs `otto` to ~/.cargo/bin
otto                          # watches ~/.ottobot/skills
```

Requires `claude` on `PATH`. macOS runs are kept awake via `caffeinate`.

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
| `runner` | `claude` (codex / opencode planned) | `--runner` or config |
| `model` | passed to the runner | runner default |
| `checker_model` | model for agent-evaluated event triggers | sonnet |
| `permission_mode` | claude `--permission-mode` | `bypassPermissions` |
| `trigger.cron` | 5- or 6-field cron | |
| `trigger.event` | prose condition, compiled to a checker script (see below) | |
| `trigger.check_every` | event poll interval | 5m |
| `trigger.cooldown` | min gap before the same condition re-fires | 8h event, 0 cron |
| `timeout` | kill the run after this | 1h |

Exactly one of `cron` / `event`.

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
otto --list                 discovered skills, hashes, parsed triggers
otto --run <skill>          one headless run, events to stdout (bypasses dedupe)
otto --show <run-id>        dump a stored run's metadata and events
otto --check <skill>        evaluate an event trigger once (compiles the checker if needed)
otto --tick 30s --runner claude
```

Keys: `tab` pane · `j/k` move · `enter` filter runs by skill / open session ·
`esc` clear filter · `r` run now · `R` reload · `l` scheduler log · `g/G`
top / follow · `q` quit.

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
runner/       Runner trait: command() + parse_line() -> RunEvent; claude impl
store.rs      ~/.ottobot/projects/<root>/ state, run metadata, event logs
state.rs      shared in-memory state between scheduler and TUI
tui/          ratatui: header (Otto + status), skills, runs, session/log
otto.rs       header art from assets/otto.ans
```

## Roadmap

1. ✅ discovery, hashing, cron triggers, claude runner, TUI, caffeinate, config
2. ✅ event triggers: compile prose → checker script via sonnet, agent fallback, dedupe key from checker output
3. `env.repo`: fetch, worktree per run on `ottobot/<skill>/<run-id>`, worktree removed after, branch kept
4. codex / opencode runners, per-runner preflight
5. headless daemon + `otto status`

## Otto art

`assets/otto.ans` is generated from the flat SVG:

```sh
RAMP=" █" cargo run -p otto-render -- assets/otto-blue.png 40 0 6 > assets/otto.ans
```

Args: source, columns, luminance weight, alpha-opening radius (strips the
propeller motion-blur arcs). A symmetric blade is drawn over the detected hub.
