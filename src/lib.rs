extern crate self as boomux;

pub mod attach;
#[cfg(feature = "benchmark-internals")]
#[doc(hidden)]
pub mod benchmark_support;
pub mod client;
pub mod conversations;
pub mod daemon;
#[doc(hidden)]
pub mod desktop_connect;
mod desktop_notifications;
mod fd_transfer;
pub mod federation;
#[allow(dead_code)]
pub mod generated_names;
#[path = "git.rs"]
#[allow(dead_code)]
mod git_metadata;
pub mod git_work;
pub(crate) mod global_workspace_store;
mod handoff;
pub mod host_services;
#[allow(dead_code)]
mod host_session_source;
#[allow(dead_code)]
mod host_session_titles;
#[allow(dead_code)]
mod integration_management;
pub mod integrations;
mod local_shell_journal;
mod node_identity;
mod node_projection;
mod node_registration;
#[doc(hidden)]
pub mod platform;
pub mod protocol;
mod session_projection;
pub mod ssh_bootstrap;
mod state_store;
mod terminal_modes;
mod terminal_state;
