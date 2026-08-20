# Rust crate design for extensible multi-crate CLI — Research #17

**Ticket:** [#17 — Rust crate design for extensible multi-crate CLI](https://github.com/Siddhj2206/cellar/issues/17) (part of [#14](https://github.com/Siddhj2206/cellar/issues/14))
**Goal:** Recommend a concrete Cargo workspace + crate graph and layering principles for `docs/rewrite-blueprint.md` that support: multi-crate split, presentation-agnostic core (future GUI crate), compile-time provider crates for runners/wrappers/components, and file-tree storage. Diliberately scoped to blueprint — not implementation.

## Landscape model

Cellar's constraints shape the workspace design space:

* **Install story:** `cargo install cellar` with no host deps required at install time; optional deps degraded via `cellar doctor`. Implies a single distributable binary assembled from the workspace.
* **First-run UX:** popup (CLI now, GUI later) that calls the same *application service* to pick prefix/runner, discover executables, register apps — so presentation must be a thin adapter over shared `app`.
* **Extensibility:** runners (managed Proton) / wrappers (umu, gamescope, mangohud) / components (DXVK). Providers are **compile-time crates**, trait-based; runtime third-party plugins out of scope. Requires a clean provider seam and DAG-friendly crate graph.
* **Source of truth:** human-editable file-tree (TOML) + disposable cache. Storage is an infra detail, not the domain.
* **Legacy layout:** single `Cargo.toml` with `[lib]`+`[[bin]]` and six `src/` modules (`cli`, `config`, `desktop`, `launch`, `runners`, `utils`). Blueprint should *virtualize* this into crates without prescribing every file.

## Crate graph pattern (Cargo workspace)

**Primary source — Cargo Book workspaces:** use a *virtual manifest* when there is no primary package and you want all packages in `crates/*`. All members share one `Cargo.lock` and one `target/` — rebuilds are shared, `cargo build/test/check --workspace` works across members, and unit features like `[patch]`/`[profile]` are inherited only from the root [doc.rust-lang.org/cargo/reference/workspaces.html](https://doc.rust-lang.org/cargo/reference/workspaces.html).

Recommended skeleton (virtual workspace, resolver 3):

```toml
# Cargo.toml (virtual)
[workspace]
resolver = "3"
members  = ["crates/*"]
exclude  = ["target", "docs"]
default-members = ["crates/cellar-cli"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.80"
license = "MIT OR Apache-2.0"
repository = "https://github.com/Siddhj2206/cellar"

[workspace.dependencies]
anyhow = "1.0"
serde  = { version = "1.0", features = ["derive"] }
tokio  = { version = "1", features = ["full"] }
# ... single source of truth for cross-crate deps

[workspace.lints.rust]
unsafe_code = "forbid"
[workspace.lints.clippy]
pedantic = "warn"
```

Members inherit with `version.workspace = true`, `dep.workspace = true`, `[lints] workspace = true` (MSRV 1.64/1.74 respectively) [doc.rust-lang.org/cargo/reference/workspaces.html](https://doc.rust-lang.org/cargo/reference/workspaces.html). Member rule: explicit `path =` deps; Cargo does **not** assume workspace members depend on each other [doc.rust-lang.org/book/ch14-03-cargo-workspaces.html](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html).

**Proposed crate graph for Cellar (DAG, strict inward deps):**

```
                    crates/cellar-cli  ──┐   presentation (clap, human output)
                    crates/cellar-gui  ──┤   future presentation (eg. iced/tauri)
                                         │
              ┌──────────────────────────┘
              ↓
       crates/cellar-app                 application: use-cases orchestrate
              ↓ depends on
       crates/cellar-core                domain: pure types, ports, invariants
              ↑
              │ implements
 crates/cellar-storage ─┐ infrastructure (file-tree TOML, cache)
crates/cellar-providers ├─ provider seam (static registry)
  ├── cellar-provider-proton
  ├── cellar-provider-wine
  ├── cellar-provider-umu
  ├── cellar-provider-gamescope
  └── cellar-provider-components

Optional crates: cellar-protocol (shared LaunchPlan DTO), cellar-testkit/fixtures, xtask
```

Dependency direction enforced by Cargo (DAG; cycles are a *compile-time error*) [aidonow.com/articles/craft/schema-registry-circular-dependency](https://www.aidonow.com/articles/craft/schema-registry-circular-dependency). If `A→B→A` is needed, extract shared `C` (e.g. `cellar-core`) instead.

*Alternative evaluated:* single `cellar` crate with `src/domain|app|infra|cli` modules (Starship-like). Works for early-stage CLIs but conflates presentation and infra deps and complicates selective compilation/testing — rejected for Cellar's GUI-future-proof goal.

## Layer rules (Clean / Hexagonal dependency rule)

Primary sources — Clean Architecture dependency rule ("dependencies point inward") [blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html via dev.to/dyarleniber/hexagonal-architecture-and-clean-architecture-with-examples-48oi](https://dev.to/dyarleniber/hexagonal-architecture-and-clean-architecture-with-examples-48oi) and Rust hexagonal port/adapter mapping where **traits = ports**, **structs = adapters** [tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html](https://tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html).

| Layer | Crate(s) | Responsibility | Allowed deps |
|-------|----------|----------------|--------------|
| **Domain** (pure) | `cellar-core` | Entities (App, Prefix, Runner), value objects, domain errors; ports as traits (e.g. `RunnerResolver`, `PrefixStore`, `WrapperContributor`); invariants & validation (no I/O, no `std::fs`, minimal deps — `serde` only for pure types if needed) | std + domain libs |
| **Application** | `cellar-app` | Use-cases/services (e.g. `FirstRunInstall`, `LaunchApp`, `DoctorCheck`); orchestrate ports; file-tree *policy* (not I/O). Owns `LaunchPlan` aggregate. | `core` only (generic over ports `R: RunnerResolver`) |
| **Infrastructure** | `cellar-storage`, `cellar-provider-*` | Implement ports: TOML file-tree read/write, cache, network fetch for managed Proton/umu; runner detection. Houses side-effects. | `core` (or `app` types) — never inverse |
| **Presentation** | `cellar-cli`, `cellar-gui` | Parse args, render output/dialogs, call one `app` entrypoint; compose concrete adapters (composition root). No business logic. | `app` + `core` DTOs, plus presentation libs (`clap`) |

Additional rules:

* **Composition root lives in `crates/cellar-cli/src/main.rs` (and later `cellar-gui`)** — only there are concrete providers instantiated and injected. Mirrors `banker-http` wiring `Bank::new(InMemoryRepo)` [tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html](https://tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html).
* **Infra swaps are additive crates, not feature flags.** Adding `cellar-provider-mangohud` doesn't touch `core`/`app`.
* **Cross-cutting concerns** (tracing, doctor checks) as decorators around ports or via `app` orchestration, not spread across layers.

## Provider seam (compile-time, trait-based)

Goal: 2–4 named providers wired at compile time, with a path to more without DAG cycles.

**1. Define ports in `core` (extendable but future-proof):**

```rust
// crates/cellar-core/src/ports.rs
pub trait RunnerResolver: Send + Sync {
    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError>;
}
pub trait WrapperContributor: Send + Sync {
    fn contribute(&self, plan: &mut LaunchPlan); // layered plan via umu
}
```

Favor `Send + Sync` (`C-SEND-SYNC`) and `Debug` (`C-DEBUG`), and keep traits object-safe when heterogeneous collections are needed (`C-OBJECT`) [rust-lang.github.io/api-guidelines/checklist.html](https://rust-lang.github.io/api-guidelines/checklist.html). When a trait is not intended for downstream impl outside the workspace, *seal* it (`pub trait Foo: private::Sealed`) — allows adding methods non-breakingly [rust-lang.github.io/api-guidelines/future-proofing.html](https://rust-lang.github.io/api-guidelines/future-proofing.html).

**2. Implement in `cellar-provider-*` crates** (one crate per runner/wrapper/component family). Keep crates narrow; share only `core`.

**3. Static registry — two idiomatic Rust choices surveyed:**

* **(Recommended for Cellar) Explicit registry crate** `cellar-providers` that *depends on each provider* and exposes `fn all_resolvers() -> Vec<Box<dyn RunnerResolver>>` or `Vec<fn() -> Box<...>>`. No magic; discoverable; test-friendly; matches the "shared schema registry as inversion point" pattern that fixes DAG cycles by extracting a zero-dep registry crate [aidonow.com/articles/craft/schema-registry-circular-dependency](https://www.aidonow.com/articles/craft/schema-registry-circular-dependency). Adding a provider = add one line + one Cargo dep.

* **(Alternative) Link-time collection via `inventory`** (`inventory::submit!` / `inventory::iter` using ELF section injection to collect impls at link time, zero runtime map) [aidonow.com/articles/craft/schema-registry-circular-dependency](https://www.aidonow.com/articles/craft/schema-registry-circular-dependency)[github.com/dtolnay/inventory]. Erases the need for a central list but is less visible in `cargo tree` and adds linker-section mechanism. Worth considering if provider count grows large and the team accepts the magic.

* **(Configuration-driven alternative) `typetag` / `serde(tag="kind")`** pattern allows `Box<dyn BlockBuilder>` to be deserialized from TOML/YAML [blog.rng0.io/how-to-easily-implement-a-configurationfirst-provider-pattern-in-rust](https://blog.rng0.io/how-to-easily-implement-a-configurationfirst-provider-pattern-in-rust). Cellar prefers file-tree on disk rather than trait-object deserialization, so `typetag` is noted but not recommended for the file-tree path.

**Compile-time vs runtime tradeoff:** generics (`struct App<R: RunnerResolver>`) give static dispatch and inlining; `Box<dyn RunnerResolver>` gives heterogeneous `Vec` (see `Screen { components: Vec<Box<dyn Draw>> }` vs `Screen<T: Draw>` discussion [doc.rust-lang.org/book/ch18-02-trait-objects.html](https://doc.rust-lang.org/book/ch18-02-trait-objects.html)). Recommend: use **generics in `app`** (test with mocks), **`Box<dyn _>` in the composition root/registry** where heterogeneity is actually needed.

**File-tree seam:** `cellar-storage` implements `PrefixStore` / `AppStore` against `~/.local/share/cellar/{configs,prefixes,runners}` using `serde + toml`. It is *driven adapter* — app calls it; it never calls app. Cache dir stays disposable.

## Proposed file layout (virtual workspace)

```
cellar/
├── Cargo.toml              # virtual manifest (workspace root)
├── Cargo.lock              # single lockfile shared by all members
├── crates/
│   ├── cellar-core/        # domain — no tokio/fs/reqwest
│   │   ├── Cargo.toml      # version.workspace=true, edition.workspace=true
│   │   └── src/{lib,ports,entities,errors}.rs
│   ├── cellar-app/         # application — depends only on core
│   │   └── src/{lib,services/{first_run,launch,doctor},launch_plan.rs}
│   ├── cellar-storage/     # infra — TOML file-tree ↔ core mapping
│   │   └── src/{lib,file_tree,repo_impl}.rs
│   ├── cellar-providers/   # registry aggregator (re-exports + all_*())
│   │   └── src/lib.rs
│   ├── cellar-provider-proton/  # each: Cargo.toml { path = ../cellar-core }
│   ├── cellar-provider-wine/
│   ├── cellar-provider-umu/
│   ├── cellar-provider-gamescope/
│   ├── cellar-cli/         # [[bin]] cellar = src/main.rs, clap derive
│   │   └── src/{main,commands,output}.rs
│   └── cellar-gui/         # future: same app entrypoint, different presentation
├── xtask/                  # cargo xtask — codegen, release checks (cargo pattern)
└── target/                 # single output dir (workspace root)
```

Conventions validated against Cargo Book: `members = ["crates/*"]` glob, `exclude` for non-member dirs, `package.workspace` override for out-of-tree members, `resolver = "3"` required on virtual manifest [doc.rust-lang.org/cargo/reference/workspaces.html](https://doc.rust-lang.org/cargo/reference/workspaces.html). `cargo new` inside a workspace auto-adds to `members`; `cargo test -p cellar-core` scopes to one crate, `--workspace` to all [doc.rust-lang.org/book/ch14-03-cargo-workspaces.html](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html).

Testing notes: each provider crate carries its own unit tests + a contract test that runs against the port trait (e.g. `run_runner_resolver_contract` mirroring `banker-fixtures` style). `cellar-app` tests inject fake in-memory adapters (`axum`-free) so `core` stays pure. CI runs `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` via `workspace.lints`.

## Examples (well-structured Rust CLI workspaces surveyed)

* **Helix editor (`helix-editor/helix`)** — canonical Rust workspace for Cellar's goals: ~13 member crates (`helix-core` pure editing logic, `helix-view` data model, `helix-tui`/`helix-term` presentations, `helix-loader` infra, `helix-lsp`/`helix-dap`, `helix-event`, `xtask`). Virtual-ish layout, strict layering, no GUI coupled into core. Blueprint should emulate Helix's `core ⟵ infra ⟵ presentation` layering and its `xtask` pattern.
* **Cargo itself (`rust-lang/cargo`)** — virtual workspace with `crates/*`, `workspace.package`/`workspace.dependencies`/`workspace.lints` inheritance, enforced by Cargo Book patterns [doc.rust-lang.org/cargo/reference/workspaces.html](https://doc.rust-lang.org/cargo/reference/workspaces.html). Validates resolver/lint/package inheritance approach.
* **codex-rs (OpenAI Codex CLI, ~70 crates)** — workspace "layered principle: user-facing entry points sit on top of shared core engine, talks down to platform/protocol layers". Demonstrates scalability of the same pattern Cellar needs (CLI now, GUI/SDK later).
* **Starship (`starship/starship`)** — counter-example: *single-crate* prompt (fast, modular inside `src/modules/`, `build.rs` codegen) with extreme configurability but not presentation-agnostic. Adopted as anti-pattern for Cellar's multi-presentation aim.
* **Small CLIs (bat, ripgrep)** — single-binary simplicity. Reference for `cargo install` binary distribution, but they don't face Cellar's provider/prefix/domain split — not a structural model to copy.

## What to avoid (costly wrong turns)

* **Starship/bat/ripgrep single-crate monolith:** forces all deps into one coherence; a GUI flag then drags `clap` into domain tests. Helix/codex-rs evidence favours split before it hurts.
* **Feature-flag providers inside one crate:** `#[cfg(feature="gamescope")]` couples compilation to Cargo features and hides edges from `cargo tree`. Prefer separate crates — features remain additive in `workspace.dependencies` but the seam is the trait, not a cfg.
* **Leaky domain:** avoid `#[derive(Serialize)]` on domain entities that include I/O concerns; prefer dedicated DTOs in `storage` that map to entities, keeping `core` free of `serde` if possible (or opt-in via optional feature).
* **Cyclic “core → storage → core”:** never add `storage` as a dep of `core` to reuse a helper — extract to `core` or a small `cellar-base`/`cellar-stdlib` instead (Rust forbids cycles; `cargo metadata` will reject it).

## Recommendation for Cellar blueprint (`docs/rewrite-blueprint.md`)

1. **Adopt a virtual workspace** at repo root with `members = ["crates/*"]`, `resolver = "3"`, `[workspace.package/dependencies/lints]` inheritance. Add `crates/cellar-cli` as `default-members` so `cargo run/test` shows CLI behavior. Define `[[bin]]` per presentation crate only; library crates expose `lib.rs`.
2. **Lock the four-layer crate graph** as above (core → app ← infra, presentation → app). State the dependency rule explicitly in the blueprint and include a diagram. Put the composition root in each presentation crate; no infra type is ever constructed inside `core`/`app`.
3. **Name 2–4 provider examples concretely:** `cellar-provider-proton` (managed GE-Proton fetch + layout), `cellar-provider-wine` (PATH/Steam discovery, no download), `cellar-provider-umu` (launch plan contributor), `cellar-provider-gamescope` (wrapper contributor). Each: trait impl + discovery + doctor hook.
4. **Choose the explicit `cellar-providers` registry** (builds on Helix-style explicit wiring; easy to audit via `cargo tree`). Document `inventory` as a *future* scaling option, and `typetag` as out-of-scope for file-tree truth.
5. **Storage contract:** `cellar-storage` is the sole TOML file-tree ↔ domain mapper; `cache/` is derived; no other crate touches `XDG_DATA` paths directly. Migrations are just file-tree transforms owned by `storage`.
6. **Cross-cutting blueprint bullets to include:** `cargo test -p <crate>` vs `--workspace`, `cargo fmt/clippy --workspace`, `xtask` for release/checks, MSRV pin via `workspace.package.rust-version`.
7. **Do not prescribe:** exact TOML schema fields, discovery heuristics, icon-extraction libs, or every future wrapper — only the seam and layering that enable them.

## Blueprint acceptance criteria (what “done” looks like)

The blueprint should be considered complete when each of the following is verifiable without writing production code:

* `cargo metadata --format-version 1 | jq` shows a DAG with `core` having no internal deps, `app → core`, `infra → core`, `presentation → app`.
* `cargo tree -p cellar-cli` lists exactly one binary and no `axum`/`tokio` leak into `core`.
* `cargo test --workspace` and `cargo clippy --workspace` commands are documented and reproduce locally via `xtask`.
* A stub `crates/cellar-providers/src/lib.rs` with `all_resolvers()` compiling against two fake providers validates the registry seam.

## Sources

* Cargo Book — Workspaces (virtual manifest, members/exclude, default-members, package/dependencies/lints inheritance, patch/profile root-only) — [doc.rust-lang.org/cargo/reference/workspaces.html](https://doc.rust-lang.org/cargo/reference/workspaces.html)
* Rust Book — Cargo Workspaces (shared Cargo.lock/target, explicit path deps, `-p` selection) — [doc.rust-lang.org/book/ch14-03-cargo-workspaces.html](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html)
* Cargo Book — Manifest Format (package metadata semantics) — [doc.rust-lang.org/cargo/reference/manifest.html](https://doc.rust-lang.org/cargo/reference/manifest.html)
* Rust API Guidelines — Checklist (C-COMMON-TRAITS, C-SEND-SYNC, C-OBJECT, C-SEALED, C-STRUCT-PRIVATE, etc.) — [rust-lang.github.io/api-guidelines/checklist.html](https://rust-lang.github.io/api-guidelines/checklist.html)
* Rust API Guidelines — Future proofing (sealed trait pattern) — [rust-lang.github.io/api-guidelines/future-proofing.html](https://rust-lang.github.io/api-guidelines/future-proofing.html)
* Trait objects & dispatch tradeoffs (generic vs `Box<dyn Trait>` heterogeneity) — [doc.rust-lang.org/book/ch18-02-trait-objects.html](https://doc.rust-lang.org/book/ch18-02-trait-objects.html)
* Helix architecture — workspace crate inventory (`helix-core`, `helix-view`, `helix-term`, `helix-tui`, `helix-loader`, `xtask`) — `helix-editor/helix` `Cargo.toml`/dir layout
* codex-rs workspace (~70 crates, layered core/platform/presentation) — OpenAI Codex Rust rewrite notes
* Starship single-crate modular monolith (counter-pattern for multi-presentation) — `starship/starship`
* Schema registry / `inventory` link-time collection pattern for DAG-friendly static provider collection — [aidonow.com/articles/craft/schema-registry-circular-dependency](https://www.aidonow.com/articles/craft/schema-registry-circular-dependency) ; `dtolnay/inventory`
* Configuration-first provider pattern with `typetag` (`#[typetag::serde(tag="kind")]`) — [blog.rng0.io/how-to-easily-implement-a-configurationfirst-provider-pattern-in-rust](https://blog.rng0.io/how-to-easily-implement-a-configurationfirst-provider-pattern-in-rust)
* Hexagonal / Clean layering in Rust (traits as ports, adapters, dependency rule, insider `banker-core → adapters → http/fixtures`) — [tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html](https://tuttlem.github.io/2025/08/31/hexagonal-architecture-in-rust.html) ; [dev.to/dyarleniber/hexagonal-architecture-and-clean-architecture-with-examples-48oi](https://dev.to/dyarleniber/hexagonal-architecture-and-clean-architecture-with-examples-48oi)
