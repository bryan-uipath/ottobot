---
name: otto
description: Distill the current conversation into an ottobot skill — a markdown file with a `trigger:` frontmatter that ottobot fires on a schedule or event. Use when the user says "/otto", "make this recurring", "turn this into a scheduled skill", "automate this investigation", or after a one-off investigation (CI failure, flaky test, nightly report) that would be worth repeating unattended.
---

# /otto — distill this conversation into a triggerable skill

You are turning work done interactively in this conversation into an
unattended job for [ottobot](https://github.com/bryan-uipath/ottobot).

## 1. Decide whether there is a skill here

Look back over the conversation for a task with all three of:

- **A trigger**: something observable that says "do this now". Either a
  schedule (nightly, every 30 min) or an event (a CI workflow failed, a PR was
  opened with a label, a Jira ticket moved to a column).
- **A repeatable procedure**: steps that would work again next time without a
  human steering, once the trigger context (run URL, ticket key) is injected.
- **A definition of done**: a triage comment, a draft PR, a Slack draft, a
  report file. Unattended runs must end in an artifact, not a conversation.

If any of the three is missing, say which one and stop. Do not invent a
trigger to make the conversation fit.

## 2. Write the skill file

Location: the user's ottobot skills root. Default `~/.ottobot/skills/`; check
`~/.ottobot/config.json` for `skills_root`. Name the file after the task
(`playwright-ci-triage.md`).

```yaml
---
name: <kebab-case>
description: <one line>
model: opus                 # opus for judgment-heavy work, sonnet for mechanical
trigger:
  cron: "0 7 * * 1-5"       # OR an event, never both
  event: "Scheduled run of <workflow url> failed"
  check_every: 5m           # event only
  cooldown: 8h              # event only; how long before the same condition re-fires
timeout: 45m
---
```

Body rules:

- Write it for an agent that has **no memory of this conversation**. Restate
  what the user already knew: repo, paths, commands, where logs live, what
  "flake vs regression" meant here, who owns what.
- Start with the goal, then numbered steps, then the exact "done" artifact and
  its format. Include a "do not" list for anything the user corrected you on.
- Reference trigger context abstractly ("the failing run URL provided in the
  trigger context"), not the specific run from this conversation.
- Never instruct the agent to push to a protected branch, merge, or send
  messages on the user's behalf. Draft PRs and draft messages only.

## 3. Show the user

Print the full file, then the trigger in one sentence ("fires when …, at most
once per 8h") and what the run will produce. Ask for one round of edits
before writing to disk if anything in step 1 was a judgment call.

## 4. Verify

```
otto --list                      # skill appears with a valid trigger
otto --run <name>                # one headless run, events to stdout
```

Report the result of `--list`. Only run `--run` if the user asks; it costs
real tokens and may act on real systems.
