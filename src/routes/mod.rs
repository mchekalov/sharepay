pub mod bill;
pub mod receipt;

pub use bill::AppState;

/// The full HTTP router: the QR/Session Management surface (spec §2.8,
/// `src/routes/bill.rs`) merged with the Receipt Recognizer's
/// `POST /b/{id}/photo` (spec §1.5, `src/routes/receipt.rs`). Split across
/// two modules by component ownership; merged here into one `axum::Router`
/// for `main.rs` to serve.
pub fn router(state: AppState) -> axum::Router {
    bill::router(state.clone()).merge(receipt::router(state))
}
