//! The loopback Responses translator that lets Codex CLI talk to a
//! chat-completions upstream.

pub mod server;
pub mod translate;

pub use server::{start, stop, CodexShim};
