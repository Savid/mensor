//! Capabilities for a hand on a real machine.

mod capture;
mod clone;
mod commands;
mod files;

pub use clone::{Cloning, Spawn, Started};
pub use commands::Commands;
pub use files::Disk;
pub use mensor_hand::Files;
