//! Shared scaffolding for the spike binaries: frame IO, the wire types, and the
//! tiny `harness` that records PASS/FAIL assertions.
//!
//! What belongs here: everything the five binaries need in common — a length-
//! prefixed frame codec on top of TOML, the two message enums, and the assertion
//! recorder. What does not: any per-binary logic (election timing, broadcast
//! fan-out, grace windows). Those live in their own files so each one stays
//! readable as a single claim.

#![allow(dead_code)]

pub mod assert;
pub mod frame;
pub mod msg;
pub mod wire;
