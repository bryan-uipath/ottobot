# ottobot tray

macOS menu-bar client for the ottobot daemon. Talks only to `~/.ottobot/otto.sock`;
schedules nothing itself. Needs a running daemon (`otto daemon --install`).

```sh
cargo install tauri-cli --version '^2' --locked   # once
cd tray/src-tauri && cargo tauri build            # → target/release/bundle/macos/ottobot tray.app
cargo tauri dev                                   # run from source
```

- **Icon**: tooltip `[K alerts · ]ottobot · N running · M failed` (daemon `Status` every 15s).
  Red dot = immediate alert, yellow = critical, else battery badge, else plain.
  Left-click toggles the popover; right-click: Run now ▸ skill, Reload skills, Open skills folder, Quit.
- **Popover**: daemon pid + root, then ALERTS (worst first: skill, age, headline, summary,
  `report ↗`, `dismiss`; hidden when none), NOW (running runs), ATTENTION (failures in 24h,
  skill parse errors), QUOTA (per runner window, `NN% left`), NEXT (skills by next fire,
  `run` per row). Polls `Snapshot` every 2s while open; hides on focus loss.

Icons: `icons/tray.png` is a 44px template (black + source alpha) rendered from `assets/otto-blue.svg`;
`tray-critical.png`/`tray-immediate.png` are `tray-warn.png` with the badge filled yellow/red and the glyph
black over a 2px white halo (not templates, so they must read on light and dark bars); the rest come from `cargo tauri icon assets/otto-blue.png`.
