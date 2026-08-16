//! App-level TOML config, read once at startup. Deliberately narrow in
//! scope: currently `ocr_engine` (selecting which
//! [`crate::receipt::ReceiptEngine`] `src/main.rs` wires up) and `currency`
//! (which symbol `src/templates.rs::fmt_cents` formats money with). Mirrors
//! `src/db/pool.rs`'s env-var-override-with-default pattern
//! (`SHAREPAY_CONFIG_PATH` / [`DEFAULT_CONFIG_PATH`]), but for a config file
//! instead of a DB path. Deliberately does NOT absorb `SHAREPAY_DB_PATH`/
//! `SHAREPAY_BASE_URL`/`SHAREPAY_TESSDATA_PREFIX` — those stay as env vars.

use std::env;
use std::path::Path;

use serde::Deserialize;

/// Env var overriding the config file path. Falls back to
/// [`DEFAULT_CONFIG_PATH`] when unset.
pub const CONFIG_PATH_ENV_VAR: &str = "SHAREPAY_CONFIG_PATH";

/// Default TOML config file, relative to the process's working directory.
pub const DEFAULT_CONFIG_PATH: &str = "sharepay.toml";

/// Which [`crate::receipt::ReceiptEngine`] backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OcrEngineKind {
    #[default]
    Tesseract,
    Claude,
}

/// Currency `src/templates.rs::fmt_cents` formats money in. Defaults to
/// Kazakhstani tenge — this app's actual target market (see `main.rs`'s
/// `OCR_LANGUAGE` doc comment and the OCR pipeline's Cyrillic keyword
/// sets, both tuned for real Kazakhstani receipts) — rather than USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Currency {
    #[default]
    Kzt,
    Usd,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub ocr_engine: OcrEngineKind,
    #[serde(default)]
    pub currency: Currency,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("failed to parse config file {path}: {source}")]
    Parse { path: String, source: toml::de::Error },
}

impl AppConfig {
    /// Loads from `SHAREPAY_CONFIG_PATH` (or [`DEFAULT_CONFIG_PATH`]).
    /// A missing file is not an error — it just means defaults
    /// (`ocr_engine = tesseract`). A malformed file is an error, so
    /// startup fails fast rather than silently ignoring a typo.
    pub fn load() -> Result<Self, ConfigError> {
        let path =
            env::var(CONFIG_PATH_ENV_VAR).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
        Self::load_from(Path::new(&path))
    }

    fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_tesseract_when_field_absent() {
        assert_eq!(
            toml::from_str::<AppConfig>("").unwrap().ocr_engine,
            OcrEngineKind::Tesseract
        );
    }

    #[test]
    fn parses_claude_engine() {
        assert_eq!(
            toml::from_str::<AppConfig>("ocr_engine = \"claude\"")
                .unwrap()
                .ocr_engine,
            OcrEngineKind::Claude
        );
    }

    #[test]
    fn missing_file_returns_defaults_not_error() {
        let cfg = AppConfig::load_from(Path::new("/nonexistent/sharepay.toml")).unwrap();
        assert_eq!(cfg.ocr_engine, OcrEngineKind::Tesseract);
        assert_eq!(cfg.currency, Currency::Kzt);
    }

    #[test]
    fn defaults_to_kzt_when_field_absent() {
        assert_eq!(toml::from_str::<AppConfig>("").unwrap().currency, Currency::Kzt);
    }

    #[test]
    fn parses_usd_currency() {
        assert_eq!(
            toml::from_str::<AppConfig>("currency = \"usd\"").unwrap().currency,
            Currency::Usd
        );
    }

    #[test]
    fn malformed_file_is_an_error() {
        let path = std::env::temp_dir().join(format!("sharepay-cfg-test-{}", std::process::id()));
        std::fs::write(&path, "ocr_engine = not valid toml [[[").unwrap();
        let result = AppConfig::load_from(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
    }
}
