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
Removing an AppEntry: Cellar runs the app's Windows uninstaller when one is registered in the prefix, then deletes the entry and its overrides; without a registered uninstaller it degrades to entry removal only. Cellar deletes no files itself — the uninstaller cleans the app's files, or they stay. Uninstalling the last AppEntry in a prefix offers to delete the prefix too, which removes everything inside it.
_Avoid_: unregister, remove, delete

**Uninstaller**:
The Windows-side removal program for an app, recorded in the prefix's registry (Add/Remove Programs). Cellar invokes it through the runner's `uninstaller --list/--remove`; a missing Uninstaller entry is what degrades Uninstall to entry removal.
_Avoid_: uninstall program, remover

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