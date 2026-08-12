//! SharePay: library crate exposing the DB pool setup and Price
//! Distributor API. `src/main.rs` is a thin binary entry point on top of
//! this; other components (QR/Session management, Receipt Recognizer,
//! Mobile Client) are expected to add their own modules alongside these
//! and wire HTTP routes on top of `pricing::api`.

pub mod db;
pub mod pricing;
