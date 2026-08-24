# Using Cellar

Cellar is a Windows app/game runtime for Linux: it keeps Wine prefixes, manages Proton and umu
runners for you, registers the executables you care about as launcher entries, and runs everything
through one uniform launch pipeline.

The three ideas worth knowing up front:

- **Prefix** — a Cellar-managed Windows environment holding defaults (runner, graphics, …) for
  every app inside it.
- **App entry** — one registered `.exe` you can launch by slug. Identity is the exe's absolute
  path; re-installing it updates the same entry.
- **Runner** — what actually runs the exe: GE-Proton and umu-launcher (installed *by* Cellar), or
  system wine / Steam Proton (discovered from your system, never modified).

Full vocabulary lives in [`CONTEXT.md`](../CONTEXT.md). This guide covers daily use; scripting
contracts are in [`cli-json.md`](cli-json.md).

## Getting started

Build and put the binary on `PATH` (desktop entries execute it directly):

```bash
cargo build --release
cp target/release/cellar ~/.local/bin/
```

There is no setup step: the first command creates Cellar's state directory
(`~/.local/share/cellar`, or `$XDG_DATA_HOME/cellar`) with sensible defaults. Grab a managed runner
and install something:

```bash
cellar runner install proton GE-Proton11-5    # check upstream releases for current tags
cellar install ~/Downloads/game_setup.exe
```

## Installing apps — `cellar install <path>`

One flow handles all three artifact kinds:

- **standalone** — a bare `.exe`: registered without being executed.
- **installer** — a Windows installer (`setup.exe`-style): run inside the chosen prefix through the
  normal launch pipeline; its output is captured to a log and its exit code decides whether the
  session continues.
- **archive** — a zip: extracted into the prefix's `drive_c`.

Which kind is asked exactly once when run interactively, with a default hinted from the file name
(`setup.exe` → installer, `.zip` → archive); the hint is always announced, never silently guessed.
Pass `--artifact standalone|installer|archive` to skip the question.

Interactive prompts appear only when stdin is a terminal and `--no-input` is absent. Every prompt
has a flag equivalent:

| Decision | Prompt | Flags |
| --- | --- | --- |
| Which prefix | Pick an existing one (numbered), type a new name, or empty for `default` | `--prefix <name>` |
| Artifact kind | Asked once, filename-hinted | `--artifact <kind>` |
| Display name | From the exe file name by default | `--name "<display name>"` |
| Kind metadata | game or tool | `--kind game\|tool` |
| Keep which discovered exes | Numbered keep/hide review + manual adds | `--keep <n>` (repeatable), `--keep-all`, `--add <path>` (repeatable) |

After an installer or archive runs, Cellar decodes the Start Menu/Desktop shortcuts created inside
the prefix and offers their targets as candidates. Nothing registers without confirmation — no
guessed "main" executable, ever. The session ends with a summary of what was registered and the
command to run each entry:

```
Registered 1 entry in prefix 'default':
  game — …/drive_c/users/me/Desktop/game.exe (game)
Run it with:
  cellar launch game
```

Re-running `cellar install` on an already-registered exe updates that same entry (use `--name` to
rename it; without `--name` the display name stays).

## Launching apps — `cellar launch <app>`

```bash
cellar launch balatro                  # foreground; the game's exit code is passed through raw
cellar launch balatro --detach         # spawn, print pid + log path, return immediately
cellar launch balatro --dry-run        # print the plan; nothing executes
cellar launch balatro --dry-run --json # the same plan as machine-readable JSON
cellar launch balatro -- -fullscreen   # arguments after -- go to the app
```

Output is captured to a per-launch log under `cache/launch-logs/` and mirrored to the terminal in
foreground mode. A game killed by a signal exits `1` with the signal reported on stderr.

The plan is resolved in a fixed order — app override → prefix default → kind default — and a failed
Proton selection never silently falls back to wine. `--dry-run` shows exactly what would run,
including the runner path and environment; paste `--dry-run --json` output when reporting launch
bugs.

## Listing and removing — `cellar list`, `cellar uninstall`

`cellar list` shows every registered app: slug, kind, prefix, runner, and status. A status of
`missing-exe` means the registered executable was deleted or moved on disk — re-register it or
uninstall the entry.

`cellar uninstall <slug>` removes the entry and its launcher integration. **Cellar never deletes
the app's own files.**

## Prefixes — `cellar prefix`

```bash
cellar prefix create my-games     # slugified; clashes dedupe as my-games-2
cellar prefix list                # defaults shown; --json available
cellar prefix delete my-games     # removes exactly this prefix's directory
```

Each prefix is a directory under `prefixes/<slug>/` containing a hand-editable `prefix.toml` next to
the actual Wine environment (`drive_c`). Defaults apply to every app bound to the prefix unless the
app overrides them:

```toml
schema_version = 1

slug = "my-games"

[defaults]
# Pin a runner by path, or rely on discovery/managed installs:
runner = { family = "Proton" }
# Wrap launches in gamescope:
graphics = "gamescope"
windows_version = "win10"
```

Hand edits are safe by contract: invalid files are skipped and flagged by `cellar doctor`, never
silently rewritten.

## Runners — `cellar runner`

```bash
cellar runner install proton GE-Proton11-5
cellar runner install umu 1.4.4
cellar runner list
```

Managed installs download resumably into the disposable cache, verify against the published
SHA-512 where one exists, extract, probe, and record in the authoritative inventory
(`runtime/providers.toml`). Versions are explicit pins — there is no guessed "latest"; installing
an already-installed version is a no-op, and a wiped `runtime/` can be restored by reinstalling the
recorded versions.

Downloads show phase progress — `Downloading GE-Proton11-5  143.2 MB / 402.1 MB (36%)`, then
`Verifying SHA-512…` and `Extracting…` — on **stderr**, repainted in place while they run. Piping
stdout keeps yielding clean data, piping stderr prints no progress at all, and `--quiet` silences it;
the final `Installed … at …` line stays on stdout as usual.

`cellar runner list` merges two worlds: managed installs from the inventory, and discover-only host
state — system wine and `umu-run` on `PATH`, plus Steam's Proton builds — shown strictly read-only.
Cellar never modifies anything it discovers.

For Proton-family apps, launching goes through `umu-run` automatically: Cellar sets `GAMEID`,
`WINEPREFIX`, `PROTONPATH`, and `PROTON_VERB` and lets umu handle its Steam Linux Runtime plumbing.
If a prefix sets `graphics = "gamescope"`, gamescope wraps the launch outermost.

## Desktop integration — `cellar desktop sync`

Registering an app creates a desktop launcher entry (`cellar-<slug>.desktop`) with an icon
extracted natively from the executable; uninstalling removes it. The **Open with Cellar**
file-manager action on `.exe` files starts the same guided install flow as the CLI.

`cellar desktop sync` re-derives all of it — entries, icons, the association — from stored state and
prunes stale entries left behind by renames or removals. It reads app state and never writes back
into the tree. Icons live in the disposable cache; deleting any of `cache/` is always safe.

## Doctor — `cellar doctor`

Four sections, in order: **tree health**, **exe integrity**, **runner integrity**, **plan
buildable**. Each finding names the item, describes the problem, and prints a fix hint. Doctor is
read-only — it reports, never repairs, and never creates state.

The exit code is the verdict: `0` healthy, `1` problems. That makes `cellar doctor` usable as a
scriptable health check.

## Scripting Cellar

- Exit codes: `0` success · `1` operation error · `2` usage. `launch` propagates the game's exit
  code raw instead.
- `-q/--quiet` works everywhere (before or after the subcommand) and silences narration only — data
  and errors still print.
- `--json` on `list`, `prefix list`, `runner list`, `doctor`, and `launch --dry-run`; shapes are
  contractual and documented in [`cli-json.md`](cli-json.md).
- Color appears only on a terminal without `NO_COLOR`; piped stdout is always plain.
- `-h/--help` and `--version` work on every command; help leads with examples.

## Where Cellar keeps its files

Everything lives under one root — `$XDG_DATA_HOME/cellar`, i.e. `~/.local/share/cellar` by default:

```
~/.local/share/cellar/
├── settings.toml              # global settings (defaults floor)
├── prefixes/<slug>/           # one directory per prefix: prefix.toml + drive_c
├── apps/<slug>.toml           # one file per registered app entry
├── runtime/
│   ├── providers.toml         # authoritative inventory of managed runners
│   ├── proton/<version>/
│   └── umu/<version>/
└── cache/                     # disposable — always safe to delete
    ├── downloads/             # resumable partial downloads
    ├── icons/                 # extracted icons
    └── launch-logs/
```

Launcher entries live beside the data root in `$XDG_DATA_HOME/applications/`. Everything except
`cache/` is authoritative state and hand-editable TOML; unknown fields are ignored, and breaking
changes carry a `schema_version`. Deleting a prefix removes exactly that prefix's directory;
uninstalling removes exactly that app's file.

## Design intent vs current behavior

A few glossary-level behaviors are specified but not built yet — described here so expectations are
honest:

- **Uninstall** currently removes the entry only. Running the app's own Windows uninstaller, and
  offering prefix deletion when the last app goes away, are designed but not implemented.
- **MangoHud** exists as a wrapper concept in the design but has no implementation yet; only the
  umu container and gamescope wrappers are active.
- **DXVK/VKD3D components**, a GUI presentation, and further runner support are future work — see
  the README roadmap note and the ADRs.
