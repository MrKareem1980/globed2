#![feature(sync_unsafe_cell, duration_constructors)]
#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::missing_safety_doc,
    clippy::wildcard_imports,
    clippy::redundant_closure_for_method_calls
)]

pub mod bridge;
pub mod data;
pub mod managers;
pub mod server;
pub mod server_thread;
pub mod state;
pub mod util;
pub mod webhook;
