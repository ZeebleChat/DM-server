use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct VoiceRoom {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub name: String,
    pub created_at: chrono::DateTime<Utc>,
    pub participant_count: i64,
}

#[derive(Debug, Serialize)]
pub struct VoiceParticipant {
    pub user_id: Uuid,
    pub joined_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct JoinVoiceRequest {
    pub channel_id: Uuid,
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct LeaveVoiceRequest {
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct GetTokenRequest {
    pub room_name: String,
    pub user_id: Uuid,
    pub username: String,
}

pub async fn list_voice_rooms(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<VoiceRoom>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT vr.id, vr.channel_id, vr.name, vr.created_at,
                (SELECT COUNT(*) FROM voice_participants WHERE room_id = vr.id) AS participant_count
         FROM voice_rooms vr",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(|r| VoiceRoom {
        id: r.get("id"),
        channel_id: r.get("channel_id"),
        name: r.get("name"),
        created_at: r.get("created_at"),
        participant_count: r.get("participant_count"),
    }).collect()))
}

pub async fn get_participants(
    State(state): State<Arc<AppState>>,
    Path(room_id): Path<Uuid>,
) -> Result<Json<Vec<VoiceParticipant>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT user_id, joined_at FROM voice_participants WHERE room_id = $1",
    )
    .bind(room_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(|r| VoiceParticipant {
        user_id: r.get("user_id"),
        joined_at: r.get("joined_at"),
    }).collect()))
}

pub async fn join_voice(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<JoinVoiceRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let room_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM voice_rooms WHERE channel_id = $1",
    )
    .bind(payload.channel_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let room_id = match room_id {
        Some(id) => id,
        None => {
            sqlx::query_scalar(
                "INSERT INTO voice_rooms (channel_id, name) VALUES ($1, $2) RETURNING id",
            )
            .bind(payload.channel_id)
            .bind(format!("Voice Channel {}", payload.channel_id))
            .fetch_one(&state.db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        }
    };

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM voice_participants WHERE room_id = $1 AND user_id = $2)",
    )
    .bind(room_id)
    .bind(payload.user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !exists {
        sqlx::query(
            "INSERT INTO voice_participants (room_id, user_id) VALUES ($1, $2)",
        )
        .bind(room_id)
        .bind(payload.user_id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let mut connections = state.connections.write().await;
    connections.voice_rooms.entry(room_id).or_default().insert(payload.user_id.to_string());
    drop(connections);

    let _: Result<(), _> = state.redis.clone()
        .set_ex(
            format!("voice:{}:{}", room_id, payload.user_id),
            Utc::now().to_rfc3339(),
            300u64,
        )
        .await;

    Ok(Json(serde_json::json!({
        "room_id": room_id,
        "channel_id": payload.channel_id,
        "success": true
    })))
}

pub async fn leave_voice(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LeaveVoiceRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let room_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT room_id FROM voice_participants WHERE user_id = $1",
    )
    .bind(payload.user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for room_id in room_ids {
        sqlx::query(
            "DELETE FROM voice_participants WHERE room_id = $1 AND user_id = $2",
        )
        .bind(room_id)
        .bind(payload.user_id)
        .execute(&state.db)
        .await
        .ok();

        // Remove empty rooms
        sqlx::query(
            "DELETE FROM voice_rooms WHERE id = $1
             AND NOT EXISTS (SELECT 1 FROM voice_participants WHERE room_id = $1)",
        )
        .bind(room_id)
        .execute(&state.db)
        .await
        .ok();

        let mut connections = state.connections.write().await;
        if let Some(room) = connections.voice_rooms.get_mut(&room_id) {
            room.remove(&payload.user_id.to_string());
        }
        drop(connections);

        let _: Result<(), _> = state.redis.clone()
            .del(format!("voice:{}:{}", room_id, payload.user_id))
            .await;
    }

    Ok(Json(serde_json::json!({ "success": true })))
}

#[derive(Serialize, Deserialize)]
struct VideoGrant {
    #[serde(rename = "roomJoin")]
    room_join: bool,
    room: String,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
}

#[derive(Serialize, Deserialize)]
struct LiveKitClaims {
    iss: String,
    sub: String,
    name: String,
    exp: u64,
    nbf: u64,
    video: VideoGrant,
}

pub async fn get_livekit_token(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<GetTokenRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let room_name = format!("voice-{}", payload.room_name);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let claims = LiveKitClaims {
        iss: state.livekit_api_key.clone(),
        sub: payload.user_id.to_string(),
        name: payload.username,
        exp: now + 3600,
        nbf: 0,
        video: VideoGrant {
            room_join: true,
            room: room_name.clone(),
            can_publish: true,
            can_subscribe: true,
        },
    };

    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(state.livekit_api_secret.as_bytes()),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "token": token,
        "url": state.livekit_url,
        "room": room_name
    })))
}
