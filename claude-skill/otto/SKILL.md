---
name: otto
description: Author an ottobot skill — a markdown file with a `trigger:` frontmatter that ottobot runs on a cron or event. Use when the user says "/otto", "make this recurring", "create an ottobot skill", "run X every morning", "monitor Y and open PRs", or after a one-off investigation worth repeating unattended. Covers cron syntax, event triggers, model routing (`model: [codex/gpt-6-astra@medium, claude/opus@medium]`), timeouts, and the HTML report convention.
---

# /otto — author an ottobot skill

Two entry points, same output:

- **From a description** ("check the nightly Playwright run every morning and open PRs").
- **From this conversation** (distill a one-off investigation into a repeatable job).

## 1. Make sure there is a skill here

Every skill needs all three. If one is missing, ask one question; don't invent it.

- **Trigger**: a schedule (cron) or an observable event (a workflow failed, a PR got a label, a Jira ticket moved).
- **Procedure**: steps that work without a human, once trigger context (run URL, ticket key) is injected.
- **Done artifact**: a draft PR, a report at `$OTTO_REPORT`, a Slack draft, a file. Unattended runs end in an artifact, not a conversation.

## 2. Frontmatter reference

```yaml
---
name: playwright-nightly            # kebab-case; defaults to file stem
description: one line
model: [codex/gpt-6-astra@medium, claude/opus@medium]   # see Models
runner: claude                      # default runner for bare model names (claude | codex)
checker_model: sonnet               # event triggers only: model that compiles/evaluates the check
permission_mode: bypassPermissions  # claude --permission-mode; codex: anything else = `-s <sandbox>`
trigger:
  cron: "0 6 * * *"                 # OR event — never both
  event: "Latest scheduled run of <workflow url> failed"
  check_every: 5m                   # event only
  cooldown: 8h                      # event only; min gap before the same condition re-fires
timeout: 90m                        # kill the run after this (default 1h)
---
```

### Cron

5 fields `min hour dom month dow` (6 with leading seconds), local time.

| want | cron |
|---|---|
| 6:00 every day | `0 6 * * *` |
| 5:30 weekdays | `30 5 * * 1-5` |
| every 15 min | `*/15 * * * *` |
| hourly at :05 | `5 * * * *` |
| Monday 9:00 | `0 9 * * 1` |
| never (manual via `r`) | `0 0 31 2 *` |

Cron dedupes by occurrence: one fire per matching minute, no cooldown by default.

### Events

Write the condition as a concrete, scriptable fact: a workflow URL, a label, a JQL. On first check `checker_model` compiles it to a script that prints `{"fired", "key", "summary"}`; `key` dedupes, `summary` becomes the run's trigger context. Verify with `otto --check <name>`.

### Models

`model` is one candidate or a list. Each candidate is `runner/model@effort`; runner and effort optional.

- `opus` → default runner, model opus. `codex/gpt-6-astra` → codex. `@low|medium|high` → reasoning effort.
- A **list** routes to the runner with the most quota left at fire time (tightest window; ties keep order). The pick is logged: `routed to codex/gpt-6-astra (93% left vs claude 70%)`.
- Aliases from `~/.ottobot/config.json` `"models": {"tier1": [...]}` → `model: tier1`.

Pick by job: judgment-heavy investigation → `[codex/gpt-6-astra@medium, claude/opus@medium]`; mechanical/cheap checks → `claude/haiku` or `claude/sonnet`. Check live quota with `otto --usage`.

Model names: claude aliases `opus | sonnet | haiku`; codex IDs from `~/.codex/models_cache.json` (e.g. `gpt-6-astra`, `gpt-6.1-sol`).

### Reports

Every run gets `$OTTO_REPORT` = `~/.ottobot/projects/<root>/reports/<run-id>.html`. If the skill should produce a report, say so in the body ("write a single self-contained HTML report to `$OTTO_REPORT`"). When the file exists at the end, the TUI shows `⎙ report` (`o` opens it) and the tray shows a `report` link.

## 3. Write the body

For an agent with **no memory of this conversation**:

1. Goal in one sentence.
2. Context it would otherwise have to rediscover: repo path, commands, where logs/artifacts live, who owns what, what "flake vs regression" means here.
3. Numbered steps. Reference trigger context abstractly ("the run URL in the trigger context").
4. The done artifact and its exact format (draft PR title/body shape, report sections).
5. A "do not" list: never push to protected branches, never merge, never send messages — drafts only.

## 4. Write, show, verify

Location: skills root — `~/.ottobot/config.json` `skills_root`, else `~/.ottobot/skills/`. File `<name>.md`.

Print the file, then one sentence: when it fires, what it will produce, which models it may use. Then:

```
otto --list                 # appears with a valid trigger
otto --check <name>         # event only: compile + evaluate once
otto --usage                # quota the router will see
```

Only `otto --run <name>` if the user asks; it costs tokens and acts on real systems. If a daemon is running, `--run` goes through it and the run shows up in the TUI/tray.
