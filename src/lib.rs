pub mod attach;
pub mod client;
pub mod daemon;
// The replacement-daemon protocol will activate this transport in the next
// handoff slice; keeping it compiled now prevents the unsafe boundary drifting.
#[allow(dead_code)]
mod fd_transfer;
mod handoff;
pub mod protocol;
mod state_store;
mod terminal_state;
