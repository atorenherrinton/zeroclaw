//! Read-only native Drive/Docs connector. Google owns remote document state.
pub mod api;
pub mod model;
pub mod operations;
pub mod protocol;
pub mod tools;

pub mod consent;
pub mod state;
pub mod write;

mod owner;
