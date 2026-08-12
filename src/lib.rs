//! SharePay: library crate exposing the DB pool setup, Price Distributor
//! API, and the QR/Session Management bill lifecycle (bill/host/participant
//! tokens, cookie auth, QR rendering, HTTP routes, retention sweep).
//! `src/main.rs` is a thin binary entry point on top of this. The Receipt
//! Recognizer and Mobile Client components are expected to add their own
//! modules alongside these.

pub mod bill;
pub mod cleanup;
pub mod db;
pub mod host_auth;
pub mod pricing;
pub mod qr;
pub mod receipt;
pub mod routes;
pub mod templates;
