// Pairing, snapshot and push APIs are used by the later cards (D6 to D8), not yet by the GUI.
#![allow(dead_code)]

pub(crate) mod devices;
pub mod protocol;
pub mod server;
mod stream;
mod subagents;
pub mod tailnet;
pub(crate) mod transcript;
mod transcript_feed;
