//! Phase 4 mechanical auto-merge classifier.
//!
//! Trust boundary: this crate runs DETERMINISTIC checks only. No LLM, no
//! network calls beyond shelling out to `cargo fmt` / `git`. The workflow
//! that invokes it must be built from `main`, not the PR branch, or the
//! whole trust story collapses (see docs/phase4-setup.md).

pub mod classify;
pub mod exit_codes;
pub mod fmt_equiv;
pub mod protected_paths;
pub mod toolchain;
pub mod verdict;
