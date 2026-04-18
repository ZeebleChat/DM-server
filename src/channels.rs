use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct Server {
    pub id: Uuid,
    pub name: String,
    pub owner_id: Uuid,
    pub icon_url: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct Channel {
    pub id: Uuid,
    pub server_id: Option<Uuid>,
    pub name: String,
    pub channel_type: String,
    pub category_id: Option<Uuid>,
    pub position: i32,
    pub topic: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
    pub owner_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub channel_type: Option<String>,
    pub category_id: Option<Uuid>,
    pub position: Option<i32>,
    pub topic: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    pub max_uses: Option<i32>,
    pub expires_in_hours: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct InviteLink {
    pub code: String,
    pub channel_id: Uuid,
    pub server_id: Option<Uuid>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    pub max_uses: Option<i32>,
    pub uses_count: i32,
}

#[derive(Debug, Serialize)]
pub struct ChannelMember {
    pub user_id: Uuid,
    pub display_name: Option<String>,
    pub nickname: Option<String>,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct ServerMember {
    pub user_id: Uuid,
    pub display_name: Option<String>,
    pub nickname: Option<String>,
    pub role: String,
}

pub async fn list_servers(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Server>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT id, name, owner_id, icon_url, created_at FROM servers ORDER BY name",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(|r| Server {
        id: r.get("id"),
        name: r.get("name"),
        owner_id: r.get("owner_id"),
        icon_url: r.get("icon_url"),
        created_at: r.get("created_at"),
    }).collect()))
}

pub async fn create_server(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateServerRequest>,
) -> Result<Json<Server>, StatusCode> {
    let row = sqlx::query(
        "INSERT INTO servers (name, owner_id) VALUES ($1, $2)
         RETURNING id, name, owner_id, icon_url, created_at",
    )
    .bind(&payload.name)
    .bind(payload.owner_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let server_id: Uuid = row.get("id");

    sqlx::query(
        "INSERT INTO server_members (server_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(server_id)
    .bind(payload.owner_id)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(Server {
        id: server_id,
        name: row.get("name"),
        owner_id: row.get("owner_id"),
        icon_url: row.get("icon_url"),
        created_at: row.get("created_at"),
    }))
}

pub async fn get_server_channels(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<Channel>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT id, server_id, name, channel_type, category_id, position, topic, created_at
         FROM channels WHERE server_id = $1 ORDER BY category_id, position, name",
    )
    .bind(server_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(row_to_channel).collect()))
}

pub async fn create_server_channel(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(payload): Json<CreateChannelRequest>,
) -> Result<Json<Channel>, StatusCode> {
    let channel_type = payload.channel_type.unwrap_or_else(|| "text".to_string());
    let row = sqlx::query(
        "INSERT INTO channels (server_id, name, channel_type, category_id, position, topic)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, server_id, name, channel_type, category_id, position, topic, created_at",
    )
    .bind(server_id)
    .bind(&payload.name)
    .bind(&channel_type)
    .bind(payload.category_id)
    .bind(payload.position.unwrap_or(0))
    .bind(&payload.topic)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(row_to_channel(&row)))
}

pub async fn list_channels(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Channel>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT id, server_id, name, channel_type, category_id, position, topic, created_at
         FROM channels ORDER BY server_id, category_id, position, name",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(row_to_channel).collect()))
}

pub async fn create_channel(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateChannelRequest>,
) -> Result<Json<Channel>, StatusCode> {
    let channel_type = payload.channel_type.unwrap_or_else(|| "text".to_string());
    let row = sqlx::query(
        "INSERT INTO channels (name, channel_type, category_id, position, topic)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, server_id, name, channel_type, category_id, position, topic, created_at",
    )
    .bind(&payload.name)
    .bind(&channel_type)
    .bind(payload.category_id)
    .bind(payload.position.unwrap_or(0))
    .bind(&payload.topic)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(row_to_channel(&row)))
}

pub async fn get_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<Channel>, StatusCode> {
    let row = sqlx::query(
        "SELECT id, server_id, name, channel_type, category_id, position, topic, created_at
         FROM channels WHERE id = $1",
    )
    .bind(channel_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(row_to_channel(&row)))
}

pub async fn delete_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(channel_id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_channel_members(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<Vec<ChannelMember>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT cm.user_id, u.display_name, sm.nickname,
                COALESCE(sm.role, 'member') AS role
         FROM channel_members cm
         JOIN users u ON cm.user_id::text = u.id
         LEFT JOIN server_members sm ON sm.server_id = (
             SELECT server_id FROM channels WHERE id = $1
         ) AND sm.user_id::text = u.id
         WHERE cm.channel_id = $1",
    )
    .bind(channel_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(|r| ChannelMember {
        user_id: r.get("user_id"),
        display_name: r.get("display_name"),
        nickname: r.get("nickname"),
        role: r.get("role"),
    }).collect()))
}

pub async fn get_server_members(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<ServerMember>>, StatusCode> {
    let rows = sqlx::query(
        "SELECT sm.user_id, u.display_name, sm.nickname, sm.role
         FROM server_members sm
         JOIN users u ON sm.user_id::text = u.id
         WHERE sm.server_id = $1
         ORDER BY sm.role, u.display_name",
    )
    .bind(server_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows.iter().map(|r| ServerMember {
        user_id: r.get("user_id"),
        display_name: r.get("display_name"),
        nickname: r.get("nickname"),
        role: r.get("role"),
    }).collect()))
}

pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(payload): Json<CreateInviteRequest>,
) -> Result<Json<InviteLink>, StatusCode> {
    let code = &Uuid::new_v4().to_string()[..8];
    let expires_at = payload.expires_in_hours.map(|h| Utc::now() + chrono::Duration::hours(h as i64));

    let server_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT server_id FROM channels WHERE id = $1",
    )
    .bind(channel_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .flatten();

    let row = sqlx::query(
        "INSERT INTO invite_links (code, channel_id, server_id, created_by, max_uses, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING code, channel_id, server_id, expires_at, max_uses, uses_count",
    )
    .bind(code)
    .bind(channel_id)
    .bind(server_id)
    .bind(Uuid::nil())
    .bind(payload.max_uses)
    .bind(expires_at)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(InviteLink {
        code: row.get("code"),
        channel_id: row.get("channel_id"),
        server_id: row.get("server_id"),
        expires_at: row.get("expires_at"),
        max_uses: row.get("max_uses"),
        uses_count: row.get("uses_count"),
    }))
}

fn row_to_channel(r: &sqlx::postgres::PgRow) -> Channel {
    Channel {
        id: r.get("id"),
        server_id: r.get("server_id"),
        name: r.get("name"),
        channel_type: r.get("channel_type"),
        category_id: r.get("category_id"),
        position: r.get("position"),
        topic: r.get("topic"),
        created_at: r.get("created_at"),
    }
}
