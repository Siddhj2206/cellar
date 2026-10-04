# Cellar

A Windows app/game runtime for Linux: Cellar provisions Wine prefixes, manages Proton and umu, discovers and registers executables, and launches them under a uniform launch plan. App-launcher-first: many apps per prefix, one launcher entry per executable.

Terms are decided through the wayfinder map [Cellar rewrite blueprint (rethink)](https://github.com/Siddhj2206/cellar/issues/14); new terms land as its domain tickets resolve.

## Language

**Cellar**:
The application the user installs and runs: the app/game launcher and Windows-runtime for Linux.
_Avoid_: wine manager, prefix manager, launcher

**AppEntry**:
A registered executable the user can launch through Cellar, identified by the canonical absolute path of its .exe and bound to exactly one prefix. Carries kind metadata and any per-app overrides of its prefix's defaults.
_Avoid_: program, game entry, shortcut, app

**kind**:
Metadata on an AppEntry — `Game` or `Tool` — that groups entries in the launcher and drives display and icons; planned as the hook for per-kind default presets (e.g. Games default to GE-Proton, Tools default to system wine). Never a storage location or a separate path.
_Avoid_: category, type, genre

**Prefix**:
A Cellar-managed Windows environment (a Wine prefix) that holds the defaults for every AppEntry inside it: runner, environment, graphics, Windows version. One prefix contains many AppEntries.
_Avoid_: bottle, environment, container

Which of those defaults the launch pipeline actually reads is a separate question from which ones the prefix stores. `runner`, `environment`, and `graphics` are all consulted when a plan is built; `windows_version` is **stored, displayed, and not yet read** — no launch plan changes because it is set (#54).

**Override**:
A per-AppEntry setting that replaces one of its prefix's defaults — including the AppEntry's own prefix binding, whose default is the prefix that registered it.
_Avoid_: setting, option, assignment

**InstallSession**:
The uniform first-run flow for any artifact — standalone exe, installer, or archive: pick or create a prefix, pick runner defaults, run the artifact, discover executables, confirm which become AppEntries. An installer is run inside the prefix (its exit awaited), an archive is extracted into the prefix, a standalone exe is registered without executing. One session touches exactly one prefix and may register zero or more AppEntries (an installer dropping five exes yields one prefix and up to five entries). Sessions are transient — they leave no history record; the durable result is the AppEntry plus its current-state metadata (runner, source installer, installed_at). The pick-or-create prefix and artifact questions are the interactive TTY flow (#32) — the same presentation entrypoint the "Open with Cellar" popup invokes for files (ADR 0004).
_Avoid_: first-run wizard, setup, install

**Discovery**:
The executable-finding step of an InstallSession: the prefix's Start Menu and Desktop shortcuts (`.lnk` files) are read and every target exe listed as a candidate; the user reviews the list — keeping, hiding, or manually adding — and kept candidates become AppEntries. Discovery never guesses a "main" exe and never auto-registers silently. (Landed in two slices: the flat scan of the menu/desktop areas in #30, `.lnk` target reading in #31.)
_Avoid_: scan, rescan, finder

**Uninstall**:
Removing an AppEntry: Cellar deletes the entry and its per-app overrides, and removes the app's launcher entry and cached icon. It deliberately spares the global "Open with Cellar" association — that belongs to the host, not to one app. Cellar deletes no app files itself — the app's own files stay until the user removes them. Two halves of the fuller flow are **design intent, not current behavior**: running the app's Windows uninstaller (glossary: Uninstaller) when one is registered in the prefix, and offering to delete the prefix when its last AppEntry goes (#44).
_Avoid_: unregister, remove, delete

**Doctor**:
The sectioned capability check (blueprint §8): tree health, exe integrity, runner integrity (managed installs), plan buildable, desktop integration (#57) — each pass/fail with a fix hint drawn from the §7 dispositions (SuggestInstall, reinstall, recreate, re-register) or, for the host-facing fifth section, the sync pointer (self-reported "kept but unrepaired" when sync cannot repair). Read-only: it reports what is on disk and never repairs — hand-edit damage surfaces with its fix, never silently overwritten (ADR 0001). Its exit code is overall health (0 healthy / 1 problems), so scripts can health-check. (Landed with #35; fifth section with #57.)
_Avoid_: diagnostics, status report, health check

**Managed runner**:
A runner Cellar provisions itself — GE-Proton / umu-Proton and umu — from a declarative manifest (source URL pattern, checksum scheme, archive layout, install kind): download (resumable), verify (SHA-512 where upstream publishes a checksum), extract, probe, and record in the authoritative `runtime/providers.toml` inventory, so the runtime directory is rebuildable at any time. Discover-only runners (system wine via PATH, Steam Proton in Steam's compatibility layout) are the read-only mirror: Cellar resolves them through the same registry but never provisions them. Resolution runs the locked order configured path → managed install → PATH / Steam (research #18). (Landed with #34.)
_Avoid_: bundled runner, downloaded runner

**Uninstaller**:
The Windows-side removal program for an app, recorded in the prefix's registry (Add/Remove Programs). **Design intent, not current behavior**: Cellar does not invoke one yet — Uninstall removes the entry and its launcher integration whatever the prefix's registry says (#44).
_Avoid_: uninstall program, remover

**CLI contract**:
The presentation's locked surface (ADR 0004, surface sweep #36): the standard flag set with consistent semantics (`-q/--quiet` global, `--json` on the data commands, `--no-input` and `-n/--dry-run` where they mean something, `--version`/`--help` on every command), examples-first help, "Did you mean?" suggestions for unknown commands and flags, exit codes 0 success / 1 operation error / 2 usage with `launch` propagating the game's code raw, and plain output under `NO_COLOR` or pipes. The `--json` shapes are contractual — see `docs/cli-json.md`.

**Launch**:
The act of running an AppEntry through its launch plan. Running an exe that is already registered launches it directly — it never re-enters the install flow; an explicit re-install action starts a new InstallSession instead.
_Avoid_: run, start, open

**LaunchPlan**:
The fully-resolved description of one launch: AppEntry, prefix, runner, environment contract, and wrappers layered around the runner.
_Avoid_: command, launch config

**Runner**:
A Wine-compatible implementation Cellar launches apps with: GE-Proton and umu-Proton (managed), system wine and Steam Proton (discovered only).
_Avoid_: engine, backend, wine version

**Wrapper**:
A layer wrapped around a runner's execution that contributes environment or behavior to a LaunchPlan: umu (the container launch layer), gamescope, mangohud.
_Avoid_: addon, overlay

**Component**:
An in-prefix library the runner uses, such as DXVK or VKD3D, installed or managed separately from the runner itself.
_Avoid_: dependency, plugin

**Provider**:
A Cellar adapter for one runner, wrapper, or component, with a mode: **Managed** (Cellar owns the inventory — downloads, verifies, versions, removes) or **Discover-only** (Cellar reads host state and owns nothing, e.g. wine on PATH, Steam Proton).
_Avoid_: plugin, driver, backend

**RunnerRef**:
A reference to a runner: the provider plus its resolved install state — a version pin for managed runners, a discovered path for system wine or Steam Proton.
_Avoid_: runner id, runner name