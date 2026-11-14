//! Node daemon: runs agent instances and talks to the control plane; containers run through `kennel`, never in-process (ADR 0007).

pub mod client;
pub mod daemon;
pub mod egress;
pub mod exec;
pub mod fleet;
