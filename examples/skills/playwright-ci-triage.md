---
name: playwright-ci-triage
description: Triage a failed scheduled Playwright CI run
runner: claude
model: opus
checker_model: sonnet
trigger:
  event: "Scheduled run of https://github.com/UiPath/flow-workbench/actions/workflows/playwright-ci.yml failed"
  check_every: 5m
  cooldown: 8h
timeout: 45m
---

A scheduled Playwright CI run failed. Find the failing tests, determine whether the failure is a flake or a regression, and write a short triage summary.
