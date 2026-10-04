# Using Cellar

Cellar is a Windows app/game runtime for Linux: it keeps Wine prefixes, manages Proton and umu
runners for you, registers the executables you care about as launcher entries, and runs everything
through one uniform launch pipeline.

The three ideas worth knowing up front:

- **Prefix** — a Cellar-managed Windows environment holding defaults (runner, graphics, …) for
  every app inside it.
- **App entry** — one registered `.exe` you can launch by slug. Identity is the exe's absolute
  path; re-installing it updates the same entry.
- **Runner** — what actually runs the exe: GE-Proton and umu-launcher (installed _by_ Cellar), or
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
cellar runner install proton GE-Proton11-5    # or just `latest` / no version for the newest release
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

| Decision                   | Prompt                                                                   | Flags                                                                |
| -------------------------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------- |
| Which prefix               | Pick an existing one (numbered), type a new name, or empty for `default` | `--prefix <name>`                                                    |
| Artifact kind              | Asked once, filename-hinted                                              | `--artifact <kind>`                                                  |
| Display name               | From the exe file name by default                                        | `--name "<display name>"`                                            |
| Kind metadata              | — (flag only; there is no kind prompt)                                   | `--kind game\|tool` (default `game` for a new entry)                  |
| Keep which discovered exes | Numbered keep/hide review + manual adds                                  | `--keep <n>` (repeatable), `--keep-all`, `--add <path>` (repeatable) |

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

A portable zip of bare executables extracts fine but creates no shortcut, so discovery finds
nothing to review. That summary names the directory to register from instead of sending you back
to the same command:

```
Registered nothing in prefix 'default' — discovery found no executable in the prefix's Start Menu/Desktop areas, so there is nothing to review. A zip of bare exes extracts without writing any shortcut — the usual cause.
Look at what the artifact left, then name the exe to register (repeat --add per exe):
  ls '~/.local/share/cellar/prefixes/default/drive_c'
  cellar install '/home/you/Downloads/bundle.zip' --prefix default --artifact archive --add '~/.local/share/cellar/prefixes/default/drive_c'/<the exe you picked>
```

The only placeholder is the exe itself — Cellar never discovered it, so it cannot name it. The rest
is the session's own artifact path and the prefix's real directory, shell-quoted. An installer that
leaves nothing behind gets the same shape with `--artifact installer` and its own explanation.

Re-running `cellar install` on an already-registered exe updates that same entry (use `--name` to
rename it; without `--name` the display name stays, and without `--kind` the entry's kind stays).

## Launching apps — `cellar launch <app>`

```bash
cellar launch balatro                  # foreground; the game's exit code is passed through raw
cellar launch balatro --detach         # spawn, print pid + log path, return immediately
cellar launch balatro --dry-run        # print the plan; nothing executes
cellar launch balatro --dry-run --json # the same plan as machine-readable JSON
cellar launch balatro -- -fullscreen   # arguments after -- go to the app
```

Output is captured to a per-launch log under `cache/launch-logs/` and mirrored to the terminal in
foreground mode. The foreground capture waits up to 500 ms after the game exits for output still in
flight; a leftover child process holding the output pipe (launcher helpers do this) ends the wait —
cellar prints `game exited; output truncated` on stderr and returns the game's exit code. A game
killed by a signal exits `1` with the signal reported on stderr.

The plan is resolved in a fixed order — app override → prefix default → kind default — and a failed
Proton selection never silently falls back to wine. `--dry-run` shows exactly what would run,
including the runner path and environment; paste `--dry-run --json` output when reporting launch
bugs.

**Order matters.** Because app arguments may start with a hyphen, Cellar stops reading its own flags
at the first one: in `cellar launch game -windowed --dry-run`, the `--dry-run` is an argument *for
the game*. Rather than launch a game you asked to preview, Cellar refuses (exit 2) and names both
orderings — put Cellar's flags first (`cellar launch game --dry-run -windowed`), or separate them
with `--` (`cellar launch game -- -windowed --dry-run`) when the app really does take that token. A
`--` anywhere on the line is taken at its word, so the second form launches the game with
`--dry-run` as its own argument.

The refusal covers every flag `launch` accepts — `--dry-run`/`-n`, `--detach`, `--json`, the global
`-q`/`--quiet`, and `-h`/`--help` and `-V`/`--version`. Arguments that are not one of those are the
game's, and pass through untouched.

## Listing and removing — `cellar list`, `cellar uninstall`

`cellar list` shows every registered app: slug, kind, prefix, runner, and status. A status of
`missing-exe` means the registered executable was deleted or moved on disk — re-register it or
uninstall the entry.

`cellar uninstall <slug>` removes the entry, its per-app overrides, and the app's launcher entry and
cached icon. **Cellar never deletes the app's own files** — they stay until you remove them.

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
(`runtime/providers.toml`). The version is a release tag; omitting it — or passing `latest` —
resolves the provider's newest published release through its feed and installs that concrete
tag (the inventory records the real tag, never "latest"). Installing an already-installed
version is a no-op, and a wiped `runtime/` can be restored by reinstalling the recorded versions.

Downloads show phase progress — `Downloading GE-Proton11-5  143.2 MB / 402.1 MB (36%)`, then
`Verifying SHA-512…` and `Extracting…` — on **stderr**, repainted in place while they run. Piping
stdout keeps yielding clean data, piping stderr prints no progress at all, and `--quiet` silences it;
the final `Installed … at …` line stays on stdout as usual.

Downloads ride one shared HTTP client: a stalled transfer (bytes stopped moving) fails after a
60-second idle timeout, transient failures — dropped connections, timeouts, `5xx`/`429` — are
retried automatically up to three attempts (1s/2s backoff, resuming from the partial download),
and a stderr line announces each retry even under `--quiet`. Proxy users are supported the
standard way: `http_proxy` / `https_proxy` / `all_proxy` / `no_proxy` environment variables are
honored; unset, Cellar connects directly.

`cellar runner list` merges two worlds: managed installs from the inventory, and discover-only host
state — system wine and `umu-run` on `PATH`, plus Steam's Proton builds — shown strictly read-only.
Cellar never modifies anything it discovers.

**Where Cellar looks for Proton** — read-only, in this order, so a multi-library Steam resolves the
same way on any host:

1. every `compatibilitytools.d` — `$XDG_DATA_HOME/Steam`, `~/.steam/steam`, and Flatpak Steam's
   `~/.var/app/com.valvesoftware.Steam/data/Steam` and `…/.local/share/Steam`;
2. the main library's `steamapps/common` (`~/.steam/steam`, and the same directory under the XDG data
   home, which is usually what that symlink resolves to);
3. every additional library Steam records in `<steam-root>/steamapps/libraryfolders.vdf`, **in the
   order that file lists them** (Steam's own priority order), reading each library's
   `compatibilitytools.d` before its `steamapps/common`.

Within one directory, Proton builds are listed name-sorted, and each is reported by its canonical
path — a build reachable through two of the roots above (`~/.steam/steam` is usually a symlink into
the XDG data home) is one row, not two. An entry only counts as a runner when it holds an executable
`proton` script. A missing or malformed `libraryfolders.vdf` simply contributes nothing: the fixed
roots still apply, and discovery never fails on it.

If a launch cannot resolve a Proton, the failure names the roots that were read rather than
suggesting an install — the usual cause is a working build on a directory Cellar was not pointed at,
and the fix is `cellar runner list` (what those roots do hold) or pinning the prefix to the install
directly with `runner = { family = "Proton", configured = { Path = "/path/to/proton" } }`.

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

Entries embed the path of the Cellar binary that wrote them. If you move or rebuild that binary,
the entries keep pointing at the old location — `cellar desktop sync` from the new location repoints
every entry and prints how many it repaired (`Repaired N stale launcher entries`), and
`cellar doctor` flags the dead ones if you haven't run it yet. An app whose file is damaged (see
Doctor) is left untouched until you repair it, so its entry is reported as kept but unrepaired.

## Doctor — `cellar doctor`

Five sections, in order: **tree health**, **exe integrity**, **runner integrity**, **plan
buildable**, **desktop integration**. Each finding names the item, describes the problem, and prints
a fix hint. Doctor is read-only — it reports, never repairs, and never creates state.

The desktop-integration section checks what Cellar derived onto your host: launcher entries whose
`Exec` target no longer exists (a moved or deleted binary) and the Open-with-Cellar association. Its
fix hint is `cellar desktop sync`, which re-derives all of it in one run — a tree with no entries
and no integration yet passes clean, and missing icons are never findings (the cache is disposable).

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

- **Uninstall** currently removes the entry, its overrides, and its launcher entry and icon. Running
  the app's own Windows uninstaller, and offering prefix deletion when the last app goes away, are
  designed but not implemented.
- **A prefix's `windows_version` default** is stored and shown by `prefix list`, but no launch plan
  reads it — the plan is identical with or without it. There is no flag for it: hand-edit
  `prefixes/<slug>/prefix.toml`, and nothing changes today (#54).
- **MangoHud** exists as a wrapper concept in the design but has no implementation yet; only the
  umu container and gamescope wrappers are active.
- **DXVK/VKD3D components**, a GUI presentation, and further runner support are future work — see
  the README roadmap note and the ADRs.
