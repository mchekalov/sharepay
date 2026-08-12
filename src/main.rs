use sharepay::db::{self, DbConfig};

/// Minimal entry point: construct the pool (running migrations on
/// startup), and start an axum server with no routes yet. The HTTP surface
/// (QR/Session management, receipt upload, mobile client fragments) is
/// owned by other components and wired in on top of this.
#[tokio::main]
async fn main() {
    let config = DbConfig::from_env();

    let pool = db::init_pool(&config)
        .await
        .expect("failed to initialize database pool / run migrations");

    // Keep the pool alive for the process lifetime; future components will
    // thread it through as axum `State`.
    let _pool = pool;

    let app = axum::Router::new();

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind to 0.0.0.0:3000");

    println!("sharepay listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app).await.expect("server error");
}
