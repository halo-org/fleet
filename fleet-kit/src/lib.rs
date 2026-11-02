//! Shared capabilities for [`fleet_core`].

#![no_std]

extern crate alloc;

mod ports;
mod types;

pub use self::{
    ports::{Queries, QueriesErrorKind},
    types::{EventDirection, TurnEndEvent},
};
