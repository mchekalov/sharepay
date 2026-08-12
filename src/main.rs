use std::sync::Arc;

use tower_http::services::ServeDir;

use sharepay::cleanup;
use sharepay::db::{self, DbConfig};
use sharepay::receipt::TesseractEngine;
use sharepay::routes::{router, AppState};

/// Environment variable naming the public base URL used to build join
/// links / QR payloads (e.g. `https://sharepay.example`). Falls back to a
/// localhost default suitable for local development only — production
/// deployments must set this to the real HTTPS domain (spec §2.1's join
/// URL shape and §2.3's `Secure` cookie both assume HTTPS in production).
const BASE_URL_ENV_VAR: &str = "SHAREPAY_BASE_URL";
const DEFAULT_BASE_URL: &str = "http://localhost:3000";

/// Directory containing `<lang>.traineddata` for the OCR engine (spec
/// §1.8). Optional — when unset, Tesseract falls back to its own
/// compiled-in default / `TESSDATA_PREFIX` env var, which on this dev
/// machine (Homebrew Tesseract on macOS) already resolves correctly to
/// `/opt/homebrew/share/tessdata` with no override needed. Set this
/// explicitly on deployment targets where that auto-resolution doesn't
/// land on the right directory (e.g. some Linux distros' packaging).
const TESSDATA_PREFIX_ENV_VAR: &str = "SHAREPAY_TESSDATA_PREFIX";

/// OCR language(s), as a `+`-joined Tesseract language string (spec §1.8
/// recommends starting with Tesseract's "fast" trained-data variant for
/// latency). Started as `eng`-only for v1; extended to also cover Russian
/// and Kazakh after validating against real Kazakhstani retail/restaurant
/// receipts, which are commonly bilingual RU/KZ Cyrillic (e.g. `ЖИЫНЫ /
/// ИТОГ`, `БАРЛЫҒЫ/ИТОГО`). Requires `rus.traineddata`/`kaz.traineddata`
/// alongside `eng.traineddata` at the resolved tessdata path (see
/// [`TESSDATA_PREFIX_ENV_VAR`]) — on this dev machine, installed via
/// `brew install tesseract-lang`. Combined multi-language recognition costs
/// measurably more CPU time than `eng`-only (see the module-level
/// benchmark note near [`sharepay::receipt::TesseractEngine`]'s call site
/// in `ocr_engine.rs`), but stayed well within an acceptable per-photo
/// budget on this dev machine, so no fallback/single-language retry path
/// was added.
const OCR_LANGUAGE: &str = "rus+kaz+eng";

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

    let tessdata_prefix = std::env::var(TESSDATA_PREFIX_ENV_VAR).ok();
    let ocr_engine: Arc<dyn sharepay::receipt::OcrEngine> = Arc::new(
        TesseractEngine::new(tessdata_prefix.as_deref(), OCR_LANGUAGE)
            .expect(
                "failed to initialize the Tesseract OCR engine — verify libtesseract/liblept \
                 and eng.traineddata are installed, or set SHAREPAY_TESSDATA_PREFIX",
            ),
    );

    let state = AppState {
        pool,
        base_url,
        ocr_engine,
    };

    let app = router(state).nest_service("/static", ServeDir::new("static"));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind to 0.0.0.0:3000");

    println!("sharepay listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app).await.expect("server error");
}
