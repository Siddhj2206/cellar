//! The Cellar application layer (blueprint §4, ADR 0003): use-cases —
//! `FirstRunInstall`, `LaunchApp`, `DoctorCheck` — thin orchestration over
//! the `core` ports, generic for static dispatch and mock testing
//! (`App<R, M, S, D>`); `Box<dyn _>` appears only at the composition root.
//!
//! Lands with slice 03 (#27: `AppEntry` registry and standalone install). The
//! scaffold ships the crate so every later slice starts from a wired,
//! rule-enforced workspace.
