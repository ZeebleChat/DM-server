use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{Json, extract::State, http::{HeaderMap, StatusCode}};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth_helpers::extract_token;
use crate::models::ErrorResponse;

#[derive(Deserialize)]
pub struct TokenRequest {
    /// The beam identity of the person being called (e.g. "Alice»1234")
    pub with: String,
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub url: String,
    pub room: String,
}

// LiveKit JWT claims (HS256)
#[derive(Serialize)]
struct LiveKitClaims {
    iss: String,
    sub: String,
    iat: u64,
    exp: u64,
    nbf: u64,
    name: String,
    video: VideoGrants,
}

#[derive(Serialize)]
struct VideoGrants {
    room: String,
    #[serde(rename = "roomJoin")]
    room_join: bool,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
    #[serde(rename = "canPublishData")]
    can_publish_data: bool,
}

/// Derive a deterministic room name from two beam identities so both
/// participants always end up in the same room.
fn room_name(a: &str, b: &str) -> String {
    let mut pair = [a, b];
    pair.sort();
    format!("dm:{}|{}", pair[0], pair[1])
}

// POST /livekit/token
pub async fn create_token(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let claims = extract_token(&*state.signing_key, &headers).await?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let room = room_name(&claims.sub, &req.with);

    let lk_claims = LiveKitClaims {
        iss: state.livekit_api_key.clone(),
        sub: claims.sub.clone(),
        iat: now,
        nbf: now,
        exp: now + 600, // 10 minutes — sufficient to join a call
        name: claims.sub.clone(),
        video: VideoGrants {
            room: room.clone(),
            room_join: true,
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
        },
    };

    let key = EncodingKey::from_secret(state.livekit_api_secret.as_bytes());
    let token = encode(&Header::new(Algorithm::HS256), &lk_claims, &key).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to generate LiveKit token".into(),
            }),
        )
    })?;

    Ok(Json(TokenResponse {
        token,
        url: state.livekit_url.clone(),
        room,
    }))
}
