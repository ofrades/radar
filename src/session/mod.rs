//! Sessions, the daemon that owns them, and the clients that attach.
//!
//! [`registry`] and [`daemon`] implement the daemon-owned runtime and the
//! snapshot-then-stream API; [`client`] attaches a client over the socket. The
//! terminal engine is libghostty-vt ([`crate::ghostty`]): the daemon parses PTY
//! output with it and encodes a lossless snapshot, and every client decodes the
//! same snapshot into its own engine. Everything here is GTK-free, so a session
//! outlives any view that renders it.

pub mod activity;
pub mod agent;
pub mod beads;
pub mod beads_store;
pub mod board;
pub mod board_store;
pub mod catalog;
pub mod client;
pub mod daemon;
pub mod dispatch;
pub mod driver;
pub mod lane;
pub mod registry;
mod schema;

/// The default grid size, before a client reports its own. Every terminal
/// starts here if nothing better is known in time.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Grid dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
}

/// How the program ended. `signal`, when present, is the readable name
/// from `strsignal` — "Terminated", "Killed", "Segmentation fault".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExitInfo {
    pub code: u32,
    pub signal: Option<String>,
}
