# Extension ports: five sealed traits; mode by membership; platform as non-seam

Status: accepted (wayfinder #21, 2026-08)

Amendment (2026-08, implemented in #28): `Storage` gains `prefix_dir()` — pure layout math the launch plan needs for the wine `WINEPREFIX` contract. Still exactly five sealed traits; sealed methods are non-breaking workspace additions, and layout knowledge stays in the adapter.

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