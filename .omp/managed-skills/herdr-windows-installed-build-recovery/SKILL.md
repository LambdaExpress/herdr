---
name: herdr-windows-installed-build-recovery
description: Recover and safely replace an installed Windows Herdr build without switching to herdr-dev state
---

## Trigger

Use when a manually installed Windows Herdr build appears to lose Spaces, config, or recent local fixes after restart.

## Diagnose

1. Inspect all running `herdr.exe` processes and their executable paths.
2. Hash and compare the installed file with checkout release/debug artifacts.
3. Run the installed binary with `--help` and inspect `Config:`.
4. Compare `%APPDATA%\herdr\session.json` with `%APPDATA%\herdr-dev\session.json` before changing either.
5. Treat `%APPDATA%\herdr\session.json` as stable state and `herdr-dev` as separate debug state. Never merge or overwrite them speculatively.

## Recover

1. Build with `cargo build --locked --release --target x86_64-pc-windows-msvc --bin herdr`; set the project-required Zig 0.15.2 path and same-drive global cache first.
2. Back up stable and debug session files plus stable config with timestamped names.
3. For an in-use installed executable, rename it to a timestamped `.previous` path, then copy the release artifact to the original installed path.
4. Verify source and installed SHA-256 hashes match.
5. Verify `herdr --help` reports `%APPDATA%\herdr\config.toml`, never `herdr-dev`.
6. Start the corrected release separately if the current agent is hosted inside the old debug server; never stop the server carrying the active session.

## Verify

- Stable server log: `persist.restore` succeeds with the expected workspace count.
- `herdr workspace list`: expected Spaces appear.
- For Windows Terminal graphics, stable config has `[experimental] kitty_graphics = true`, `WT_SESSION` is present, and the live client reports nonzero cell pixel dimensions.
- Keep renamed binaries until their old processes exit.
