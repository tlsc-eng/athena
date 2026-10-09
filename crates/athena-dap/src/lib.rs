//! A Debug Adapter Protocol client on plain threads, for Delve's `dlv dap`, and the
//! `.vscode/launch.json` subset that says what to debug.

mod client;
mod framing;
mod types;

pub use client::{Adapter, Client, Event, LAUNCH_TIMEOUT, REQUEST_TIMEOUT, Transport};
pub use types::{
    BreakpointStatus, Evaluated, Scope, SourceBreakpoint, StackFrame, Stopped, Thread, Variable,
};
