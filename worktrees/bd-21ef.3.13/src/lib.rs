//! kb — agent knowledge base CLI
//!
//! Manages agent-kb-events.jsonl (committed event log) and agent-kb.db
//! (local materialized SQLite cache). Modeled on `br` (beads).

#![deny(unsafe_code)]
#![warn(
    rust_2018_idioms,
    trivial_casts,
    unused_lifetimes,
    unused_qualifications
)]

pub mod application;
#[doc(hidden)]
pub mod bench_fixture;
pub mod commands;
pub mod components;
pub mod config;
pub mod models;
pub mod prelude;
