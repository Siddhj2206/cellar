# CLI surface: verb-first with object groups; popup as presentation mode

Status: accepted (wayfinder #23, 2026-08)

Cellar's CLI is verb-first at the top level for artifact actions — `cellar install <path>` (the flagship, installer-aware), `cellar launch <app>` (daily driver), `uninstall`, `list`, `doctor` — with two noun groups for lifecycle objects (`prefix`, `runner`). The rule for implementers: *actions on artifacts are top-level verbs; objects with lifecycle get noun groups*. `cellar install <path>` is always an artifact; managed runtimes are `cellar runner install`, so the single verb stays unambiguous. The first-run "popup" is a presentation mode of the same InstallSession application service (interactive TTY flow now, GUI dialog later), wired from file associations straight to the presentation binary — never a separate program.

## Considered options

- **Pure noun-verb surface** (docker-style `cellar app install`): rejected — buries the flagship verb two words deep and adds a noun layer to every daily action.
- **Separate popup binary / shell wrapper in the MIME exec line**: rejected — duplicates the application service and breaks the symmetric-presentation rule (#20).
- **Magic exit-code mapping for game exits**: rejected — #7 propagates the game's code raw; Cellar errors are 0/1/2 with the collision documented.

## Consequences

- The surface is contractual once muscle memory forms: top-level commands, flag names, and exit codes (0 / 1 / 2, launch passthrough) change only through deprecation, never renames (clig.dev).
- New commands slot in under one stated rule, so implementers can't invent conflicting CLIs.
- The future GUI reuses every §8 use-case entrypoint unchanged — only the chrome differs.
- Extension (2026-08, implemented in #33): the desktop-integration subsystem gets a third noun group under the rule — `cellar desktop sync` re-derives launcher entries, icons, and the Open-with-Cellar association from the tree (the "cache re-derivable at any time" contract needs an invocation surface; subsystems with no artifact/lifecycle home follow the noun-group shape). A future desktop command lands under the same group.
- The `runner` noun group's managed surface lands with #34: `runner install <provider> <version>` — the version is an explicit pin (no silent "latest": the artifact is named deterministically by its release tag), downloads resumably, verifies, extracts, probes, and records; `runner list` shows managed plus discover-only rows (read-only). The group's future verbs (`update`, `remove`) land as extensions under the same rule.
- The doctor's full surface lands with #35, in the locked §8 shape: four sections — tree health, exe integrity, runner integrity, plan buildable — each pass/fail with a fix hint (the §7 dispositions: SuggestInstall, reinstall, recreate, re-register; hand-edit damage surfaces with its fix and is never silently repaired). Exit code is overall health (0 healthy / 1 problems) so scripts can health-check; the `--json` shape mirrors the report and is audited in the surface sweep (#36).