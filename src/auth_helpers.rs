use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use jsonwebtoken::errors::{Error, ErrorKind};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

use crate::models::{AccessClaims, ErrorResponse};

pub fn decode_access_token(token: &str, signing_key: &SigningKey) -> Result<AccessClaims, Error> {
    let verifying_key = signing_key.verifying_key();
    let x_bytes = verifying_key.to_bytes();
    let x_b64 = URL_SAFE_NO_PAD.encode(x_bytes);
    let decoding_key = DecodingKey::from_ed_components(&x_b64)
        .map_err(|_| Error::from(ErrorKind::InvalidAlgorithm))?;
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.validate_exp = true;
    let data = decode::<AccessClaims>(token, &decoding_key, &validation)?;
    Ok(data.claims)
}

/// Helper to extract and decode token from Authorization header
pub async fn extract_token(
    signing_key: &SigningKey,
    headers: &HeaderMap,
) -> Result<AccessClaims, (StatusCode, Json<ErrorResponse>)> {
    let auth = headers.get("Authorization").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Missing Authorization header".into(),
            }),
        )
    })?;
    let bearer = auth.to_str().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid Authorization header".into(),
            }),
        )
    })?;
    if !bearer.starts_with("Bearer ") {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid Authorization format".into(),
            }),
        ));
    }
    let token = &bearer[7..];
    match decode_access_token(token, signing_key) {
        Ok(claims) => Ok(claims),
        Err(_) => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid or expired token".into(),
            }),
        )),
    }
}
