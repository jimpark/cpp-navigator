//! `cpp-navigator` — LLM-optimized C++ codebase navigator.
//!
//! Library crate behind the `cpp-navigator` / `cppnav` binaries. See `docs/`
//! for the PRD and design spec. The pipeline is a layered hybrid
//! (design-specs §4): candidate finder → syntactic engine → (opt-in) semantic
//! engine → text fallback → serializer.

// Scaffolding stage: modules are filled in phase-by-phase. Remove once the
// pipeline is fully wired.
#![allow(dead_code)]

pub mod cli;
pub mod commands;
pub mod engine;
pub mod interactive;
pub mod model;
pub mod output;
pub mod search;
