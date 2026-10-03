//! radar — a native workspace manager.
//!
//! The shape of the app: projects live in a sidebar, and each project owns a
//! set of tabs, each tab running one tool (editor, agent, diff, shell) in an
//! embedded terminal, plus the board — the one built-in that runs nothing.
//!
//! This library holds everything that is not GTK: the SQLite stores, the
//! program registry, argument building and directory discovery. The GUI
//! (behind the `gui` feature) is a thin layer on top so the core stays
//! testable and reusable from the CLI.

pub mod config;
pub mod db;
pub mod discover;
pub mod ghostty;
pub mod git;
#[cfg(feature = "gui")]
pub mod gui;
pub mod mcp;
pub mod programs;
pub mod session;
pub mod setup;
pub mod skill;
pub mod web;

pub use config::Paths;
pub use db::Db;
