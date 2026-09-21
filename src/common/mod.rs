//! Common types and utilities shared between TUI and hook command.

pub mod activity;
pub mod chrome;
// Claude's own per-process session registry — the first-party status source.
pub mod claude_sessions;
pub mod config;
// Shared gather: builds the ConversationRegistry for both the TUI and the web.
pub mod conversations;
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
// Per-conversation token usage, read incrementally from the transcript.
#[allow(dead_code)]
pub mod usage;
pub mod worktree;
// Git state of one working tree — what a conversation actually produced.
#[allow(dead_code)]
pub mod worktree_health;
