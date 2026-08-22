# Crate graph: domain / launch / desktop split with symmetric presentations

Status: accepted (wayfinder #20, 2026-08)

Amendment (2026-08, implemented in #25): `cellar-providers` depends on `cellar-core` in addition to the provider crates — the registry names the sealed port trait types it returns (`dyn RunnerResolver`, `dyn ManagedRunner`, `dyn WrapperContributor`), and Rust requires direct dependencies for naming. Layering intent unchanged: the registry never depends on `app`/`launch`/`storage`/`desktop`.

Second amendment (2026-08, implemented in #26): `cellar-cli` also depends on `cellar-storage` — the composition root must name the concrete adapter (`TreeStore`) to inject into the generic application services (`PrefixService<S: Storage>`), and Rust requires direct dependencies for naming. Presentation still runs no tree logic; it constructs and injects, nothing more. Layering intent unchanged: storage never depends on presentation, and `app` stays generic over the port.

Third amendment (2026-08, implemented in #28): `cellar-app` also depends on `cellar-launch` — the `LaunchApp` use-case orchestrates the plan pipeline (resolve → check → plan, blueprint §7), and Rust requires direct dependencies for naming. Layering intent unchanged: the launch machinery stays pure rules over `core` (selection precedence, chain assembly, plan assembly), `app` stays thin orchestration, and neither depends on presentation. Presentations consume `launch` through `app`; the composition root additionally composes the provider registry into a single `RunnerResolver` (the heterogeneity `Box<dyn _>` the blueprint reserves for presentation).

Fourth amendment (2026-08, implemented in #29): the execute phase lands in `cellar-launch` — spawning the frozen plan (`SpawnedProcess`, per-launch logs, foreground/detached modes) — so the pipeline's phase boundary ("nothing spawns until the plan is complete", blueprint §7) lives in one crate. The spawn is plain `std::process` machinery, no platform abstractions: the crate's dependency edge stays `core`-only, `app` orchestrates (`LaunchApp::spawn` composes the log path via the storage port) and re-exports the launch surface, and presentations keep the wait-vs-detach policy. Layering intent unchanged.

Cellar's workspace is a virtual Cargo workspace of flat, seam-prefixed crates with strict inward dependency rules: pure domain (`cellar-core`) at the center; infrastructure (`cellar-storage`, `cellar-desktop`, `cellar-provider-*`) implements `core` ports; launch machinery gets its own crate (`cellar-launch`); use-case orchestration (`cellar-app`) stays thin; presentations (`cellar-cli` now, `cellar-gui` later) are symmetric leaves that alone instantiate the provider registry (composition root). The CLI is primary today; the GUI becomes primary later — a product choice (default-members, packaging), not an architecture change.

## Considered options

- **Launch machinery inside `cellar-app`** (earlier research sketch): rejected — resolution outweighs orchestration; the dedicated `cellar-launch` keeps `app` thin and the machinery mock-testable in isolation.
- **Function-grouped crate names** (`cellar-runners/proton`, `cellar-runtime/umu` — the original #20 sketch): rejected — flat seam-prefixed names keep `cargo tree` greppable with only 4–6 provider crates.

## Consequences

- Adding a provider: new crate + registry line in `cellar-providers` + presentation dep — zero edits below presentation.
- Adding the GUI: one presentations leaf; nothing below it changes.
- Cycles are compile-time errors (Cargo DAG); `unsafe` forbidden workspace-wide; per-provider contract tests against the ports in `core`.