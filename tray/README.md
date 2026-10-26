# ottobot tray

macOS menu-bar client for the ottobot daemon. Talks only to `~/.ottobot/otto.sock`;
schedules nothing itself. Needs a running daemon (`otto daemon --install`).

```sh
cargo install tauri-cli --version '^2' --locked   # once
cd tray/src-tauri && cargo tauri build            # → target/release/bundle/macos/ottobot tray.app
cargo tauri dev                                   # run from source
```

- **Icon**: tooltip `ottobot · N running · M failed` (daemon `Status` every 15s).
  Left-click toggles the popover; right-click: Run now ▸ skill, Reload skills, Quit.
- **Popover**: daemon pid + root, then NOW (running runs), ATTENTION (failures in 24h,
  skill parse errors), QUOTA (per runner window, `NN% left`), NEXT (skills by next fire,
  `run` per row). Polls `Snapshot` every 2s while open; hides on focus loss.

Icons: `icons/tray.png` is a 44px template (black + source alpha) rendered from `assets/otto-blue.svg`;
the rest come from `cargo tauri icon assets/otto-blue.png`.
