//! RamFlow for Linux — a foreground-first resource orchestrator.
//!
//! The daemon watches memory/CPU pressure through `/proc` (including PSI), and when the
//! machine is under contention it lowers the priority of background jobs the user listed,
//! so the work the user is actually waiting on stays responsive. Every change is journaled
//! *before* it is made, and is undone when the pressure ends, when the daemon stops, or on
//! the next start after a crash.
//!
//! There are no third-party dependencies.

pub mod config;
pub mod controller;
pub mod engine;
pub mod fmt;
pub mod foreground;
pub mod journal;
pub mod policy;
pub mod proc;
pub mod provider;
pub mod sys;
