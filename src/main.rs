use sharepay::cleanup;
use sharepay::db::{self, DbConfig};
use sharepay::routes::{router, AppState};

/// Environment variable naming the public base URL used to build join
/// links / QR payloads (e.g. `https://sharepay.example`). Falls back to a
/// localhost default suitable for local development only — production
/// deployments must set this to the real HTTPS domain (spec §2.1's join
/// URL shape and §2.3's `Secure` cookie both assume HTTPS in production).
const BASE_URL_ENV_VAR: &str = "SHAREPAY_BASE_URL";
const DEFAULT_BASE_URL: &str = "http://localhost:3000";

#[tokio::main]
async fn main() {
    let config = DbConfig::from_env();

    let pool = db::init_pool(&config)
        .await
        .expect("failed to initialize database pool / run migrations");

    // Retention sweep (spec §2.6): re-spawned on every process start, not
    // relied upon to survive a restart.
    cleanup::spawn(pool.clone());

    let base_url =
        std::env::var(BASE_URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let state = AppState { pool, base_url };

    let app = router(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind to 0.0.0.0:3000");

    println!("sharepay listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app).await.expect("server error");
}
