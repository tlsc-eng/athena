//! A Debug Adapter Protocol client on plain threads, for Delve's `dlv dap`, and the
//! `.vscode/launch.json` subset that says what to debug.

mod client;
mod delve;
mod framing;
mod launch;
mod types;

pub use client::{Adapter, Client, Event, LAUNCH_TIMEOUT, REQUEST_TIMEOUT, Transport};
pub use delve::{DELVE_INSTALL_HINT, delve_adapter, find_delve};
pub use launch::{
    Context, Launch, LaunchConfig, Mode, default_config, go_configurations, substitute,
};
pub use types::{
    BreakpointStatus, Evaluated, Scope, SourceBreakpoint, StackFrame, Stopped, Thread, Variable,
};
