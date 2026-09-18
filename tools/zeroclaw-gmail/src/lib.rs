//! Direct Gmail REST integration. Provider state owns effects; the private SQLite
//! ledger owns immutable reviews, authorization, claims and receipts.
pub mod api;
pub mod auth;
pub mod model;
pub mod operations;
pub mod protocol;
pub mod store;
#[cfg(test)]
mod tests;
pub mod tools;
