---
name: hello
description: Smoke test. Fires every 2 minutes.
model: haiku
trigger:
  cron: "*/2 * * * *"
timeout: 5m
---

Say hello in one short sentence, then run `date` and report the output. Do nothing else.
