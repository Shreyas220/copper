//! Wire types shared by the controller, the agent and the CLI.
//!
//! Two protocols live here:
//! - `node`: controller <-> agent (desired state pull, observed state push)
//! - `user`: what `hc` and any future UI talk to

pub mod node;
pub mod user;

pub use node::*;
pub use user::*;
