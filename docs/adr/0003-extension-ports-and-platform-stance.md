# Extension ports: five sealed traits; mode by membership; platform as non-seam

Status: accepted (wayfinder #21, 2026-08)

Amendment (2026-08, implemented in #28): `Storage` gains `prefix_dir()` — pure layout math the launch plan needs for the wine `WINEPREFIX` contract. Still exactly five sealed traits; sealed methods are non-breaking workspace additions, and layout knowledge stays in the adapter.

Second amendment (2026-08, implemented in #29): `Storage` gains `launch_logs_dir()` — pure layout math for the execute phase's per-launch output (`cache/launch-logs`, blueprint §7: disposable). Same reasoning as `prefix_dir`: layout knowledge stays in the adapter, sealed methods are non-breaking, still exactly five sealed traits.

Third amendment (2026-10, implemented in #46): `Storage` gains `sweep_cache()` — the disposable cache's retention sweep, run opportunistically on the launch path (the one command guaranteed to run often) and nowhere else. The adapter, not `app`, owns both the cache layout and the entry set the icon liveness rule consults, which is why that rule is derived from the real icon naming instead of guessed; and the method's result is advisory by contract, because a sweep that cannot prune its cache is never a reason to fail a launch. Its blast radius is the carve-out ADR 0001 already draws: removals inside `cache/launch-logs` and `cache/icons` only, unsynced, silent.

Cellar's extension surface is exactly five sealed traits in `core::ports` — `RunnerResolver`, `ManagedRunner` (declarative `manifest()`; Managed vs Discover-only is trait membership), `WrapperContributor` (with a `Layer` enum ordering the chain; env contracts are wrapper data, not launch machinery), `Storage` (file-tree mapping, `.lnk` discovery, and the shared `Installer` pipeline under one port — one external system, one port), and `DesktopIntegrator` — implemented by the provider crates, `cellar-storage`, and `cellar-desktop`. Install strategies, components, runtime plugins, and any `Platform` abstraction are deliberately not ports. Cellar is Linux-first; a future macOS edition enters through the existing seams (additive provider crates, per-platform storage/desktop implementations), never a platform trait.

## Considered options

- **Fine-grained research split** (`PrefixStore`, `AppStore`, discoverer, installer as separate ports): rejected — swap-ability is per-crate, not per-method; six-plus traits mean more generic params and mock structs for a granularity nobody needs.
- **One runner trait with mode as data**: rejected — discover-only providers would carry `install()` stubs.
- **Per-provider install code**: rejected — four copies of the same pipeline; a declarative manifest plus a shared storage-owned installer keeps providers as pure descriptors.
- **Free-form `u8` wrapper priority**: rejected — ordering correctness would escape the domain; a `Layer` enum makes unknown layers compile errors.
- **A `Platform` port**: rejected — platform-specificity already flows through the existing seams (providers are additive; storage and desktop are already ports).

## Consequences

- Adding a managed runner = one descriptor + one registry line; adding a wrapper = one provider crate, plus a `Layer` variant only if it is a genuinely new layer kind.
- Env-contract knowledge lives in wrapper providers — the umu chain (#18) is *contributed*, not hardcoded (refines the #20 wording: `cellar-launch` is a generic chain builder).
- App generics stay contained: `App<R, M, S, D>` over the four relevant ports; `Box<dyn _>` only at the composition root.