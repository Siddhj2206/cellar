# Contributing to Cellar

Development happens on the `next` branch. Rust 1.85+ is required.

## The one-command gate

```bash
cargo xtask check
```

runs everything CI would — `fmt --check`, `clippy` (pedantic, denied workspace-wide with no
opt-outs), the full test suite, and a build across all workspace members. If it passes locally, it
passes CI.

Individual pieces:

```bash
cargo build                  # debug build
cargo test                   # full suite
cargo test <name>            # one test / module
cargo check                  # fast typecheck
cargo clippy --workspace --all-targets
cargo fmt --all
```

## Where things live

The workspace is a strict DAG of crates (`docs/adr/0002-crate-graph-and-layer-rules.md`) — layers
may only depend downward:

| Crate | Role |
| --- | --- |
| `cellar-core` | Domain entities, ports (sealed traits), errors, glossary types |
| `cellar-storage` | The tree: TOML source of truth, atomic writes, managed-runner installer |
| `cellar-launch` | Launch plan resolution and process execution |
| `cellar-provider-{wine,proton,umu,gamescope}` | One adapter per runner/wrapper |
| `cellar-providers` | The registry wiring providers together |
| `cellar-app` | Application services (install sessions, listing, doctor) over the ports |
| `cellar-desktop` | Launcher entries, icons, file association |
| `cellar-cli` | **The binary** and the only composition root — where concrete infra gets built |
| `cellar-gui` | Reserved future leaf; becomes primary via `default-members`, zero edits elsewhere |

Rules that are enforced, not aspirational: `unsafe` is forbidden workspace-wide; app services never
touch concrete infrastructure (they're generic over `cellar-core` ports); storage is the sole
writer of the tree. Design decisions and their rationale live in [`docs/adr/`](docs/adr/) — read
ADR 0001–0004 before changing storage layout, crate boundaries, provider seams, or the CLI surface;
those are contractual and change only through deprecation.

## Code style

[`AGENTS.md`](AGENTS.md) doubles as the style guide: `anyhow::Result<T>` at the presentation
boundary (sealed typed errors inside), `PathBuf` for paths, external-crate → std → local import
order, serde `#[derive(Debug, Clone, Serialize, Deserialize)]` on config structs with
`Default` impls, snake_case functions, doc comments explaining *why*.

Tests live next to the code they cover; end-to-end CLI behavior (parsing, exit codes, popup
invocations) has integration tests under `crates/cellar-cli/tests/`.

## Issues and workflow

Work is tracked in GitHub issues ([`docs/agents/issue-tracker.md`](docs/agents/issue-tracker.md)).
New issues start as `needs-triage`; fully-specified work graduates to `ready-for-agent`. Commits
reference their issue (`feat(runner): … (#34)`). User-facing behavior changes should update the
glossary in [`CONTEXT.md`](CONTEXT.md) — it is the vocabulary authority.
