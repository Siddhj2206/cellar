# `--json` output shapes

Status: **contractual** — stabilized by the CLI surface sweep (#36); scripts may depend on these shapes.

Every data command accepts `--json` and prints nothing else on stdout (errors still go to stderr).
Exit codes are identical to the human renderings: `doctor --json` exits 1 when the report has
problems, `launch --dry-run --json` is spawn-free, and parse failures exit 2 on stderr (clap).

All output is `serde_json` pretty-printed with a trailing newline. Optional fields are **omitted**
when absent (`skip_serializing_if`), except where noted — the samples below show both states.

## `cellar list --json`

One object per registered app: the entry's fields plus its current `status` (the check phase's
verdict: `"ok"` or `"missing-exe"`).

```json
[
  {
    "slug": "balatro",
    "exe": "/home/me/.local/share/cellar/prefixes/default/drive_c/games/balatro.exe",
    "kind": "game",
    "prefix": "default",
    "overrides": {},
    "status": "ok"
  }
]
```

With a pinned runner (current-state metadata) and a runner override:

```json
{
  "slug": "tool",
  "exe": "/home/me/.local/share/cellar/prefixes/work/drive_c/tools/helper.exe",
  "kind": "tool",
  "prefix": "work",
  "overrides": {
    "runner": {
      "family": "Proton"
    }
  },
  "runner": {
    "provider_id": "proton",
    "family": "Proton",
    "install": {
      "Managed": {
        "version": "GE-Proton11-5",
        "path": "/home/me/.local/share/cellar/runtime/proton/GE-Proton11-5"
      }
    }
  },
  "status": "ok"
}
```

Vocabulary (the machine tags are the serde defaults — capitalized exactly as shown, stable):

| field | values |
| --- | --- |
| `kind` | `"game"` \| `"tool"` (`rename_all = "lowercase"`) |
| `status` | `"ok"` \| `"missing-exe"` |
| `overrides.prefix` | string, omitted when absent |
| `overrides.runner` | a runner spec (below), omitted when absent |
| `overrides.env` | `{ "VAR": "value" }`, omitted when empty |
| `runner` | the pinned `RunnerRef` (below), omitted when absent |
| `source_installer`, `installed_at` | string, omitted when absent |

Runner spec: `{ "family": "Proton" \| "Wine" \| "Umu", "configured": { "Path": "<abs>" } \| { "Version": "<pin>" } }`
— `configured` omitted when absent; `family` keeps serde's default capitalization (no `rename_all`).

`RunnerRef`: `{ "provider_id": "proton", "family": "Proton" \| "Wine" \| "Umu", "install": … }` where
`install` is externally tagged: `{ "Managed": { "version": …, "path": … } }` or
`{ "Discovered": { "path": …, "version": … } }` (`version` omitted when unknown).

## `cellar prefix list --json`

```json
[
  {
    "slug": "default",
    "defaults": {
      "runner": {
        "family": "Wine",
        "configured": {
          "Path": "/home/me/.local/share/cellar/prefixes/default/wine"
        }
      }
    }
  },
  {
    "slug": "work",
    "defaults": {
      "runner": null,
      "graphics": "gamescope"
    }
  }
]
```

Note: `defaults.runner` serializes as `null` when unset (no `skip_serializing_if` on `Prefix`'s
`Option` fields); `env` is omitted when empty; `graphics`/`windows_version` omitted when absent.

## `cellar runner list --json`

One object per runner: managed installs from the authoritative inventory plus discover-only host
state. `mode` is `"managed"` or `"discover-only"`; `version` is a string or `null`.

```json
[
  {
    "mode": "managed",
    "provider": "proton",
    "version": "GE-Proton11-5",
    "path": "/home/me/.local/share/cellar/runtime/proton/GE-Proton11-5"
  },
  {
    "mode": "discover-only",
    "provider": "umu",
    "version": null,
    "path": "/usr/bin/umu-run"
  }
]
```

## `cellar doctor --json`

The report mirrors the human sections exactly: `healthy` is the overall verdict (exit code 0/1),
`findings` is empty on a passing section, and each finding carries its fix hint.

```json
{
  "healthy": false,
  "sections": [
    {
      "name": "tree health",
      "healthy": true,
      "findings": []
    },
    {
      "name": "exe integrity",
      "healthy": false,
      "findings": [
        {
          "item": "balatro",
          "problem": "registered executable missing from disk",
          "fix": "re-register it (`cellar install <path>`) or `cellar uninstall balatro`"
        }
      ]
    }
  ]
}
```

Sections, in locked order: `tree health`, `exe integrity`, `runner integrity`, `plan buildable`,
`desktop integration` (#57 added the fifth). The desktop-integration section reports launcher
entries whose Exec target no longer exists and a missing or dead Open-with-Cellar association; an
entry whose app file is damaged (#56) self-reports instead of the sync hint:

```json
{
  "name": "desktop integration",
  "healthy": false,
  "findings": [
    {
      "item": "cellar-icon32.desktop",
      "problem": "its Exec target no longer exists",
      "fix": "run cellar desktop sync to re-point it"
    }
  ]
}
```

## `cellar launch <app> --dry-run --json`

The serialized launch plan — a reproducible artifact for bug reports (blueprint §7). `--json`
requires `--dry-run` (usage error otherwise); nothing spawns.

```json
{
  "argv": [
    "/home/me/.local/share/cellar/runtime/umu/1.4.4/umu-run",
    "/home/me/.local/share/cellar/prefixes/default/drive_c/games/balatro.exe"
  ],
  "env": {
    "GAMEID": "umu-balatro",
    "PROTONPATH": "/home/me/.local/share/cellar/runtime/proton/GE-Proton11-5",
    "PROTON_VERB": "waitforexitandrun",
    "WINEPREFIX": "/home/me/.local/share/cellar/prefixes/default"
  },
  "wrappers": ["Container"]
}
```

`cwd` is omitted when the plan needs none; `wrappers` lists the chain outermost-first with the
serde-default capitalized tags: `"Display"` (gamescope) → `"Container"` (umu) → `"RuntimeEnv"`.

## Stability contract

These shapes are contractual (ADR 0004): scripts may parse them. Fields are added over time;
renames or removals happen only through deprecation, never silently. The same rules apply to the
human output contract — exit codes, `--quiet`, and `NO_COLOR` — documented in ADR 0004.