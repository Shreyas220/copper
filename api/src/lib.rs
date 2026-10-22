//! Wire types shared by the controller, the agent, the CLI and the console.
//!
//! - `node`: controller <-> agent (desired state pull, observed state push)
//! - `user`: instances, projects and nodes as `cu` and the console see them
//! - `account`: sign-in, invites, users and tokens
//! - `ops`: the event log, metrics, quotas and fit checks

pub mod account;
pub mod node;
pub mod ops;
pub mod user;

pub use account::*;
pub use node::*;
pub use ops::*;
pub use user::*;
