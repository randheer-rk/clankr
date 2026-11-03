//! Node daemon: runs agent instances (handshake, delivery, call brokering) and talks to the
//! control plane. Containers are run through `kennel`, never in-process (ADR 0007).

pub mod client;
pub mod daemon;
pub mod egress;
pub mod exec;
pub mod fleet;
