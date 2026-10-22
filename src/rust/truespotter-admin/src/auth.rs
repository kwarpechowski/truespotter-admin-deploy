//! Admin auth — port of AdminAuthExtensions.cs (login + request guard).
//!
//! POST /api/admin/login: x-admin-secret compared against METRICS_SECRET in
//! constant time; on success issues an HS256 JWT signed with JWT_SECRET
//! carrying the `admin` role (ClaimTypes.Role → the long Microsoft claim
//! URI, exactly like JwtSecurityToken) with a 1h expiry, and responds
//! `{token, expiresIn: 3600}`.
//!
//! The guard on GET endpoints mirrors UseAdminAuth: a Bearer JWT signed
//! with JWT_SECRET carrying the admin role, OR the x-admin-secret header
//! (or ?key= query) matching METRICS_SECRET. Zitadel-issued admin JWTs
//! (JWKS validation) are NOT ported — deviation, see README report.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::Serialize;

use subtle::ConstantTimeEq;

use crate::{problem, text, AppState};

/// ClaimTypes.Role — the URI JwtSecurityTokenHandler writes for role claims.
const ROLE_CLAIM: &str = "http://schemas.microsoft.com/ws/2008/06/identity/claims/role";
const TOKEN_TTL_SECONDS: i64 = 3600;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub token: String,
    pub expires_in: i64,
}

/// True when the decoded JWT payload carries the admin role under the
/// Microsoft ClaimTypes.Role URI (as issued by JwtSecurityToken) or a
/// plain "role" claim.
fn has_admin_role(claims: &serde_json::Value) -> bool {
    claims.get(ROLE_CLAIM).and_then(|v| v.as_str()) == Some("admin")
        || claims.get("role").and_then(|v| v.as_str()) == Some("admin")
}

/// Constant-time UTF-8 byte comparison (CryptographicOperations.FixedTimeEquals
/// requires equal lengths — .NET returns false for length mismatch too).
pub fn fixed_time_equals(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.ct_eq(b).into()
}

/// POST /api/admin/login (AdminAuthExtensions.MapAdminLogin).
pub async fn login_handler(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let (Some(metrics_secret), Some(jwt_secret)) = (&state.metrics_secret, &state.jwt_secret)
    else {
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "METRICS_SECRET or JWT_SECRET is not set",
        );
    };

    let Some(provided) = headers.get("x-admin-secret").and_then(|v| v.to_str().ok()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if provided.is_empty() || !fixed_time_equals(provided, metrics_secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let exp = (Utc::now() + Duration::hours(1)).timestamp();
    let claims = serde_json::json!({
        ROLE_CLAIM: "admin",
        "exp": exp,
    });
    let token = match encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    ) {
        Ok(t) => t,
        Err(_) => return problem(StatusCode::INTERNAL_SERVER_ERROR, "token encoding failed"),
    };

    Json(LoginResponse {
        token,
        expires_in: TOKEN_TTL_SECONDS,
    })
    .into_response()
}

/// Axum middleware applying `admin_guard` to every protected route
/// (UseAdminAuth port). Mounted on the read endpoints in build_router.
pub async fn admin_middleware(
    State(state): State<std::sync::Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = req.headers().clone();
    let key = req.uri().query().and_then(|q| {
        q.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == "key").then(|| String::from_utf8_lossy(&urldecode(v)).into_owned())
        })
    });
    match admin_guard(&state, &headers, key.as_deref()) {
        Ok(()) => next.run(req).await,
        Err(resp) => *resp,
    }
}

/// Minimal %XX url-decode for the ?key= fallback.
fn urldecode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (
                    (bytes[i + 1] as char).to_digit(16),
                    (bytes[i + 2] as char).to_digit(16),
                ) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// UseAdminAuth port: Bearer JWT (JWT_SECRET, admin role) OR x-admin-secret
/// OR ?key=. Err(boxed) → the exact .NET failure response; Ok(()) → proceed.
pub fn admin_guard(
    state: &AppState,
    headers: &HeaderMap,
    key_query: Option<&str>,
) -> Result<(), Box<Response>> {
    if state.metrics_secret.as_deref().unwrap_or("").is_empty() {
        return Err(Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "METRICS_SECRET is not set",
            )
                .into_response(),
        ));
    }

    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("Bearer "));

    if let Some(bearer) = bearer {
        if let Some(jwt_secret) = state.jwt_secret.as_deref().filter(|s| !s.is_empty()) {
            let token = bearer["Bearer ".len()..].trim();
            let mut validation = Validation::new(Algorithm::HS256);
            validation.validate_aud = false;
            validation.iss = None;
            // .NET TokenValidationParameters: no required claims beyond
            // lifetime (a token without exp passes as non-expiring).
            validation.required_spec_claims.clear();
            match decode::<serde_json::Value>(
                token,
                &DecodingKey::from_secret(jwt_secret.as_bytes()),
                &validation,
            ) {
                Ok(data) if has_admin_role(&data.claims) => return Ok(()),
                _ => {
                    // fall through to the shared-secret branch (Zitadel
                    // JWKS validation is not ported — deviation).
                }
            }
        }
    }

    let provided = headers
        .get("x-admin-secret")
        .and_then(|v| v.to_str().ok())
        .or(key_query);

    match provided {
        Some(p)
            if !p.is_empty()
                && fixed_time_equals(p, state.metrics_secret.as_deref().unwrap_or("")) =>
        {
            Ok(())
        }
        _ => Err(Box::new(text(StatusCode::UNAUTHORIZED, "Unauthorized"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_time_equals_matches_and_rejects() {
        assert!(fixed_time_equals("sekret", "sekret"));
        assert!(!fixed_time_equals("sekret", "Sekret"));
        assert!(!fixed_time_equals("sekret", "sekret2"));
        assert!(!fixed_time_equals("", "x"));
        assert!(fixed_time_equals("", ""));
    }

    fn roundtrip(secret: &str) -> (String, serde_json::Value) {
        let exp = (Utc::now() + Duration::hours(1)).timestamp();
        let claims = serde_json::json!({ ROLE_CLAIM: "admin", "exp": exp });
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        validation.iss = None;
        validation.required_spec_claims.clear();
        let decoded = decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        )
        .unwrap();
        (token, decoded.claims)
    }

    #[test]
    fn issued_jwt_has_ms_role_uri_claim_and_admin_role() {
        let (_, claims) = roundtrip("jwt-secret");
        assert_eq!(claims[ROLE_CLAIM].as_str(), Some("admin"));
        assert!(has_admin_role(&claims));
    }

    #[test]
    fn issued_jwt_expires_in_one_hour() {
        let before = (Utc::now() + Duration::hours(1) - Duration::seconds(5)).timestamp();
        let (_, claims) = roundtrip("jwt-secret");
        let after = (Utc::now() + Duration::hours(1) + Duration::seconds(5)).timestamp();
        let exp = claims["exp"].as_i64().unwrap();
        assert!(exp > before && exp < after);
    }

    #[test]
    fn expired_jwt_fails_validation() {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &serde_json::json!({ ROLE_CLAIM: "admin", "exp": (Utc::now() - Duration::hours(1)).timestamp() }),
            &EncodingKey::from_secret("jwt-secret".as_bytes()),
        )
        .unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        validation.iss = None;
        validation.required_spec_claims.clear();
        assert!(decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret("jwt-secret".as_bytes()),
            &validation
        )
        .is_err());
    }

    #[test]
    fn wrong_signature_fails_validation() {
        let (token, _) = roundtrip("jwt-secret");
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        validation.iss = None;
        validation.required_spec_claims.clear();
        assert!(decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret("other-secret".as_bytes()),
            &validation
        )
        .is_err());
    }

    #[test]
    fn token_without_admin_role_rejected() {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &serde_json::json!({ ROLE_CLAIM: "visitor", "exp": (Utc::now() + Duration::hours(1)).timestamp() }),
            &EncodingKey::from_secret("jwt-secret".as_bytes()),
        )
        .unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        validation.iss = None;
        validation.required_spec_claims.clear();
        let decoded = decode::<serde_json::Value>(
            &token,
            &DecodingKey::from_secret("jwt-secret".as_bytes()),
            &validation,
        )
        .unwrap();
        assert!(!has_admin_role(&decoded.claims));
    }
}
