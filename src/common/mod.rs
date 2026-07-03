//! Common types and utilities shared between TUI and hook command.

pub mod activity;
pub mod chrome;
pub mod debug;
pub mod frozen;
// Shared multi-Claude-per-session detection (consumed by the web server; TUI next).
pub mod instances;
pub mod iterm;
#[allow(dead_code)]
pub mod jsonl;
pub mod machine;
pub mod persistence;
pub mod ports;
pub mod process;
pub mod projects;
// Read-only shadow model (Increment 0); wired into views in a later increment.
#[allow(dead_code)]
pub mod registry;
pub mod tmux;
pub mod types;
pub mod worktree;
