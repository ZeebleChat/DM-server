use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct Message {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_id: Uuid,
    pub content: String,
    pub message_type: String,
    pub attachments: Option<serde_json::Value>,
    pub reply_to: Option<Uuid>,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct GetMessagesQuery {
    limit: Option<i64>,
    before: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub author_id: Uuid,
    pub content: String,
    pub message_type: Option<String>,
    pub attachments: Option<serde_json::Value>,
    pub reply_to: Option<Uuid>,
}

pub async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    let limit = query.limit.unwrap_or(50).min(100);

    let rows = if let Some(before_id) = query.before {
        sqlx::query(
            "SELECT id, channel_id, author_id, content, message_type, attachments, reply_to,
                    created_at, updated_at
             FROM channel_messages
             WHERE channel_id = $1 AND id < $2 AND deleted_at IS NULL
             ORDER BY created_at DESC LIMIT $3",
        )
        .bind(channel_id)
        .bind(before_id)
        .bind(limit)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query(
            "SELECT id, channel_id, author_id, content, message_type, attachments, reply_to,
                    created_at, updated_at
             FROM channel_messages
             WHERE channel_id = $1 AND deleted_at IS NULL
             ORDER BY created_at DESC LIMIT $2",
        )
        .bind(channel_id)
        .bind(limit)
        .fetch_all(&state.db)
        .await
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(row_to_message).collect()))
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<Message>, StatusCode> {
    if payload.content.is_empty() && payload.attachments.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let message_type = payload.message_type.unwrap_or_else(|| "text".to_string());

    let row = sqlx::query(
        "INSERT INTO channel_messages
             (channel_id, author_id, content, message_type, attachments, reply_to)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, channel_id, author_id, content, message_type, attachments,
                   reply_to, created_at, updated_at",
    )
    .bind(channel_id)
    .bind(payload.author_id)
    .bind(&payload.content)
    .bind(&message_type)
    .bind(&payload.attachments)
    .bind(payload.reply_to)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let message = row_to_message(&row);

    let _: Result<(), _> = state.redis.clone()
        .set_ex(
            format!("channel:{}:last_message", channel_id),
            message.id.to_string(),
            86400u64,
        )
        .await;

    Ok(Json(message))
}

pub async fn delete_message(
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    sqlx::query(
        "UPDATE channel_messages SET deleted_at = NOW() WHERE id = $1 AND channel_id = $2",
    )
    .bind(message_id)
    .bind(channel_id)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

fn row_to_message(r: &sqlx::postgres::PgRow) -> Message {
    Message {
        id: r.get("id"),
        channel_id: r.get("channel_id"),
        author_id: r.get("author_id"),
        content: r.get("content"),
        message_type: r.get("message_type"),
        attachments: r.get("attachments"),
        reply_to: r.get("reply_to"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
