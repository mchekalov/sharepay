//! Host and participant cookie issuance/verification (spec §2.3/§2.4).
//!
//! Uses a plain (unencrypted, unsigned) `axum_extra::extract::cookie::CookieJar`
//! rather than `PrivateCookieJar`. Justification: the cookie *value* is
//! itself a 128-bit CSPRNG bearer token (the participant token or the raw
//! host token) that the server always re-validates against the database on
//! every request (a hash comparison for the host token; a row-existence
//! check for the participant id) — encrypting the cookie would add a
//! second secret-management concern (a server-side signing key) without
//! adding real security, since forging a *valid* cookie value still
//! requires guessing a 128-bit token either way. `HttpOnly` (blocks JS
//! access/XSS exfiltration), `Secure` (HTTPS-only transport), and
//! `SameSite=Lax` (basic CSRF mitigation for a cookie that grants no
//! destructive cross-site-forgeable action beyond what `SameSite=Lax`
//! already blocks) are set explicitly per spec §2.3/§2.4.

use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use sqlx::SqlitePool;
use time::Duration as CookieDuration;

use crate::bill;
use crate::bill::token::hash_token;

/// Cookie `Max-Age`. Deliberately generous relative to the actual bill TTL
/// (pre-open bills expire in 2h, open bills auto-close by 24h, closed bills
/// are retained 7 days — spec §2.6): the cookie outliving the bill row is
/// harmless (the bill lookup will simply 404/expired), whereas a cookie
/// that expires *before* the bill does would prematurely log a host/
/// participant out of a still-live bill. 30 days comfortably covers every
/// case above with margin.
pub const COOKIE_MAX_AGE: CookieDuration = CookieDuration::days(30);

/// Errors from host-cookie verification. Deliberately collapsed to one
/// variant at the HTTP layer (spec §2.7: "Never distinguish missing vs.
/// mismatched in the response") — kept as an enum here only so call sites
/// can log/trace the real reason without leaking it to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAuthError {
    BillNotFound,
    CookieMissing,
    TokenMismatch,
}

pub fn host_cookie_name(bill_id: &str) -> String {
    format!("sharepay_host_{bill_id}")
}

pub fn participant_cookie_name(bill_id: &str) -> String {
    format!("sharepay_participant_{bill_id}")
}

/// Builds a `HttpOnly; Secure; SameSite=Lax; Path=/b/<bill_id>` cookie per
/// spec §2.3/§2.4.
fn build_cookie(name: String, value: String, bill_id: &str) -> Cookie<'static> {
    Cookie::build((name, value))
        .path(format!("/b/{bill_id}"))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(COOKIE_MAX_AGE)
        .build()
}

/// Adds the host cookie (raw `host_token`, never the hash) to the jar.
pub fn set_host_cookie(jar: CookieJar, bill_id: &str, host_token: &str) -> CookieJar {
    jar.add(build_cookie(
        host_cookie_name(bill_id),
        host_token.to_string(),
        bill_id,
    ))
}

/// Adds the participant cookie (the participant's own 128-bit id) to the
/// jar.
pub fn set_participant_cookie(jar: CookieJar, bill_id: &str, participant_id: &str) -> CookieJar {
    jar.add(build_cookie(
        participant_cookie_name(bill_id),
        participant_id.to_string(),
        bill_id,
    ))
}

/// Reads the participant id cookie for this bill, if present. Does *not*
/// verify the participant still exists in the DB — callers that need that
/// guarantee (e.g. `mark_item`) get it for free since `pricing::api`
/// re-validates participant/bill membership on every mutation (spec §3.5).
pub fn participant_id_from_jar(jar: &CookieJar, bill_id: &str) -> Option<String> {
    jar.get(&participant_cookie_name(bill_id))
        .map(|c| c.value().to_string())
}

/// Verifies the jar carries a valid host cookie for `bill_id`: present, and
/// its SHA-256 hash matches `bills.host_token_hash`. Missing and mismatched
/// tokens return distinct enum variants for internal logging, but every
/// HTTP call site must map all of them to the same generic 401-equivalent
/// response (spec §2.7).
pub async fn verify_host(
    pool: &SqlitePool,
    jar: &CookieJar,
    bill_id: &str,
) -> Result<(), HostAuthError> {
    let stored_hash = bill::get_host_token_hash(pool, bill_id)
        .await
        .map_err(|_| HostAuthError::BillNotFound)?
        .ok_or(HostAuthError::BillNotFound)?;

    let cookie_value = jar
        .get(&host_cookie_name(bill_id))
        .map(|c| c.value().to_string())
        .ok_or(HostAuthError::CookieMissing)?;

    if hash_token(&cookie_value) == stored_hash {
        Ok(())
    } else {
        Err(HostAuthError::TokenMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bill::token::generate_token;
    use crate::db::{init_pool, DbConfig};
    use crate::pricing::api::create_bill;

    async fn test_pool() -> SqlitePool {
        init_pool(&DbConfig::in_memory()).await.unwrap()
    }

    #[tokio::test]
    async fn verify_host_accepts_matching_token_and_rejects_others() {
        let pool = test_pool().await;
        let host_token = generate_token();
        let bill_id = "host-auth-bill";
        create_bill(&pool, bill_id, &hash_token(&host_token))
            .await
            .unwrap();

        let jar = CookieJar::new();
        let ok_jar = set_host_cookie(jar, bill_id, &host_token);
        assert!(verify_host(&pool, &ok_jar, bill_id).await.is_ok());

        let empty_jar = CookieJar::new();
        assert_eq!(
            verify_host(&pool, &empty_jar, bill_id).await,
            Err(HostAuthError::CookieMissing)
        );

        let wrong_jar = CookieJar::new();
        let wrong_jar = set_host_cookie(wrong_jar, bill_id, "not-the-real-token");
        assert_eq!(
            verify_host(&pool, &wrong_jar, bill_id).await,
            Err(HostAuthError::TokenMismatch)
        );

        let missing_bill_jar = CookieJar::new();
        let missing_bill_jar = set_host_cookie(missing_bill_jar, "no-such-bill", &host_token);
        assert_eq!(
            verify_host(&pool, &missing_bill_jar, "no-such-bill").await,
            Err(HostAuthError::BillNotFound)
        );
    }

    #[test]
    fn participant_id_from_jar_reads_the_per_bill_cookie() {
        let jar = CookieJar::new();
        let jar = set_participant_cookie(jar, "bill-1", "participant-token-abc");
        assert_eq!(
            participant_id_from_jar(&jar, "bill-1").as_deref(),
            Some("participant-token-abc")
        );
        assert_eq!(participant_id_from_jar(&jar, "bill-2"), None);
    }
}
