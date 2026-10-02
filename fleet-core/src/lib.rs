//! An AI agent harness.

#![no_std]

extern crate alloc;

#[cfg_attr(not(test), expect(dead_code, reason = "consumer not yet implemented"))]
mod task;
