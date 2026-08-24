# Cellar

A Windows app/game runtime for Linux. Cellar provisions Wine prefixes, manages Proton and umu,
discovers and registers Windows executables, and launches them through one uniform command —
app-launcher-first: many apps per prefix, one launcher entry per executable.

> Status: early development, CLI-only. A GUI is planned and will drive the same engine.

## Features

- **Guided installs** — `cellar install <path>` handles standalone executables, Windows installers,
  and zip archives through one flow: pick or create a prefix, run or extract the artifact, review
  what it dropped, register the entries worth keeping.
- **One-word launches** — `cellar launch <app>`, foreground or `--detach`, with `--dry-run` printing
  the exact plan (a reproducible bug report).
- **Managed runners** — Cellar downloads and maintains GE-Proton and umu-launcher itself: resumable
  downloads, checksum verification, explicit version pins. System wine and Steam Proton installs are
  discovered automatically and treated strictly read-only.
- **Desktop integration** — launcher entries with icons extracted straight from the executables, plus
  an "Open with Cellar" action for `.exe` files in your file manager. `cellar desktop sync`
  re-derives all of it from the stored state.
- **Inspectable state** — everything lives in hand-editable TOML under one directory; the cache is
  disposable; `cellar doctor` reports problems with fix hints and never repairs silently.

## Requirements

- Linux on x86_64 or aarch64
- [Rust](https://rustup.rs) 1.85+ (to build)
- Optional: system `wine` on `PATH` (used for tools), `gamescope`, a Steam install (its Proton
  builds are detected automatically)

There is nothing else to set up by hand — GE-Proton and umu-launcher are installed and managed by
Cellar itself.

## Building from source

```bash
git clone https://github.com/Siddhj2206/cellar.git
cd cellar
cargo build --release
cp target/release/cellar ~/.local/bin/   # put it on PATH; desktop entries exec it directly
```

## Quick start

```bash
cellar runner install proton GE-Proton11-5     # a managed runner (check upstream for current tags)
cellar install ~/Downloads/game_setup.exe      # guided when run in a terminal
cellar list                                    # what's registered
cellar launch game                             # run it
```

Any Cellar command creates its state directory on first use — there is no setup step.

## Command overview

| Command | Purpose |
| --- | --- |
| `cellar install <path>` | Guided install of a standalone exe, installer, or archive |
| `cellar launch <app>` | Launch a registered app (`--dry-run`, `--detach`) |
| `cellar uninstall <app>` | Remove an entry (never deletes the app's own files) |
| `cellar list` | Registered apps with status |
| `cellar doctor` | Sectioned checks with fix hints; exit code = health |
| `cellar prefix create \| list \| delete` | Manage Wine prefixes |
| `cellar runner install \| list` | Managed runners; discover-only host runners shown read-only |
| `cellar desktop sync` | Re-derive launcher entries, icons, and the file association |

Every command leads its `-h` output with examples; unknown commands and typos get suggestions.
Scripted use: `--json` on data commands, global `-q/--quiet`, exit codes `0` success / `1` operation
error / `2` usage, plain output under `NO_COLOR` or pipes.

## Documentation

- [Usage guide](docs/usage.md) — the full manual: installing, launching, runners, doctor, storage
  layout, scripting.
- [`docs/cli-json.md`](docs/cli-json.md) — contractual `--json` shapes for scripts.
- [`CONTEXT.md`](CONTEXT.md) — domain glossary; [`docs/adr/`](docs/adr/) — design decisions.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Development happens on the `next` branch; `cargo xtask
check` is the one-command gate (fmt + clippy + tests + build).
