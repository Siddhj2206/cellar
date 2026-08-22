//! The Cellar launch machinery (blueprint §7): launch-plan resolution —
//! runner ref (configured → managed → PATH), wrapper-chain assembly sorted by
//! `Layer`, the `resolve → check → plan → execute` pipeline. A generic chain
//! builder: env contracts are wrapper-provider data, never launch machinery
//! (#21, ADR 0003).
//!
//! Lands with slices 04/05 (#28: resolution and dry-run, #29: execute and
//! process semantics). The scaffold ships the crate so every later slice
//! starts from a wired, rule-enforced workspace.
