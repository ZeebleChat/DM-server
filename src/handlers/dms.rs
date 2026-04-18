use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        multipart::Multipart,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::broadcast;
use tracing::error;

use crate::AppState;
use crate::auth_helpers::{decode_access_token, extract_token};
use crate::models::{
    Attachment, AttachmentUploadResponse, DirectMessage, DmListQuery, ErrorResponse, SendDmRequest,
    UploadResult,
};

/// Look up a user's UUID by their beam identity (format: "display_name»tag").
/// Returns None if not found.
async fn get_user_id_by_beam(pool: &PgPool, beam_identity: &str) -> Option<String> {
    let parts: Vec<&str> = beam_identity.split('»').collect();
    if parts.len() != 2 {
        return None;
    }
    let display_name = parts[0];
    let beam_tag = parts[1];
    sqlx::query_scalar("SELECT id FROM users WHERE display_name = $1 AND beam_tag = $2")
        .bind(display_name)
        .bind(beam_tag)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

// Attachments
const ALLOWED_MIME_TYPES: [&str; 14] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "video/mp4",
    "video/webm",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "application/pdf",
    "text/plain",
    "text/markdown",
    "application/zip",
];
const MAX_FILE_SIZE: i64 = 10 * 1024 * 1024; // 10MB

/// Sanitize filename: remove path components, limit length
fn sanitize_filename(filename: &str) -> String {
    let sanitized = filename.replace("..", "").replace(['/', '\\'], "_");
    if sanitized.len() > 255 {
        if let Some(ext_pos) = sanitized.rfind('.') {
            let ext = &sanitized[ext_pos..];
            let max_name_len = 255 - ext.len();
            if max_name_len > 0 {
                format!("{}{}", &sanitized[..max_name_len], ext)
            } else {
                sanitized[..255].to_string()
            }
        } else {
            sanitized[..255].to_string()
        }
    } else {
        sanitized
    }
}

// GET /dms?with=...&limit=...
pub async fn list_dms(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<DmListQuery>,
) -> Result<Json<Vec<DirectMessage>>, (StatusCode, Json<ErrorResponse>)> {
    let claims = extract_token(&*state.signing_key, &headers).await?;
    let with = query.with;
    if with.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Missing 'with' parameter".into(),
            }),
        ));
    }
    let limit = query.limit.unwrap_or(100).min(1000) as i64;

    let partner_id = match get_user_id_by_beam(&state.db, &with).await {
        Some(id) => id,
        None => return Ok(Json(Vec::new())),
    };
    let current_user_id = claims.uid.clone();
    let current_user_beam = claims.sub.clone();

    let rows = sqlx::query(
        "SELECT m.id, m.sender_beam, m.recipient_beam, m.content, m.created_at,
                a.id AS att_id, a.filename, a.mime_type, a.file_size
         FROM dm_messages m
         LEFT JOIN attachments a ON m.id = a.dm_message_id
         WHERE (m.sender_id IS NOT NULL AND m.recipient_id IS NOT NULL AND
                ((m.sender_id = $1 AND m.recipient_id = $2) OR (m.sender_id = $2 AND m.recipient_id = $1)))
            OR ((m.sender_id IS NULL OR m.recipient_id IS NULL) AND
                ((m.sender_beam = $3 AND m.recipient_beam = $4) OR (m.sender_beam = $4 AND m.recipient_beam = $3)))
         ORDER BY m.created_at ASC, a.id ASC
         LIMIT $5",
    )
    .bind(&current_user_id)
    .bind(&partner_id)
    .bind(&current_user_beam)
    .bind(&with)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database error".into(),
            }),
        )
    })?;

    let mut msgs: Vec<DirectMessage> = Vec::new();
    let mut current_message_id: Option<i64> = None;
    let mut current_attachments: Vec<Attachment> = Vec::new();
    let mut current_message: Option<DirectMessage> = None;

    for row in rows {
        let msg_id: i64 = row.try_get("id").unwrap_or(0);
        let sender_beam: String = row.try_get("sender_beam").unwrap_or_default();
        let recipient_beam: String = row.try_get("recipient_beam").unwrap_or_default();
        let content: String = row.try_get("content").unwrap_or_default();
        let created_at: String = row.try_get("created_at").unwrap_or_default();
        let att_id: Option<i64> = row.try_get("att_id").unwrap_or(None);
        let att_filename: Option<String> = row.try_get("filename").unwrap_or(None);
        let att_mime_type: Option<String> = row.try_get("mime_type").unwrap_or(None);
        let att_file_size: Option<i64> = row.try_get("file_size").unwrap_or(None);

        if let Some(prev_id) = current_message_id {
            if prev_id != msg_id {
                if let Some(msg) = current_message.take() {
                    msgs.push(DirectMessage {
                        id: msg.id,
                        sender_beam: msg.sender_beam,
                        recipient_beam: msg.recipient_beam,
                        content: msg.content,
                        created_at: msg.created_at,
                        attachments: current_attachments.clone(),
                    });
                }
                current_attachments.clear();
            }
        }

        current_message_id = Some(msg_id);
        current_message = Some(DirectMessage {
            id: msg_id,
            sender_beam,
            recipient_beam,
            content,
            created_at,
            attachments: Vec::new(),
        });

        if let (Some(aid), Some(filename), Some(mime_type), Some(file_size)) =
            (att_id, att_filename, att_mime_type, att_file_size)
        {
            current_attachments.push(Attachment {
                id: aid,
                filename,
                mime_type,
                file_size,
            });
        }
    }

    if let Some(msg) = current_message.take() {
        msgs.push(DirectMessage {
            id: msg.id,
            sender_beam: msg.sender_beam,
            recipient_beam: msg.recipient_beam,
            content: msg.content,
            created_at: msg.created_at,
            attachments: current_attachments,
        });
    }

    Ok(Json(msgs))
}

// POST /dms
pub async fn send_dm(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendDmRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let claims = extract_token(&*state.signing_key, &headers).await?;
    let to = req.to.trim().to_string();
    let content = req.content.trim().to_string();
    let attachment_ids = req.attachment_ids.clone();
    if content.is_empty() && attachment_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Empty message".into(),
            }),
        ));
    }

    let recipient_id = match get_user_id_by_beam(&state.db, &to).await {
        Some(uid) => uid,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Recipient user not found".into(),
                }),
            ));
        }
    };

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO dm_messages (sender_beam, recipient_beam, sender_id, recipient_id, content, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW()::TEXT)
         RETURNING id",
    )
    .bind(&claims.sub)
    .bind(&to)
    .bind(&claims.uid)
    .bind(&recipient_id)
    .bind(&content)
    .fetch_one(&state.db)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to store message".into(),
            }),
        )
    })?;

    let created_at: String = sqlx::query_scalar(
        "SELECT created_at FROM dm_messages WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .unwrap_or_default();

    let attachments = if !attachment_ids.is_empty() {
        let rows_affected = sqlx::query(
            "UPDATE attachments SET dm_message_id = $1
             WHERE id = ANY($2) AND uploaded_by = $3 AND dm_message_id IS NULL",
        )
        .bind(id)
        .bind(&attachment_ids)
        .bind(&claims.uid)
        .execute(&state.db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);

        if rows_affected != attachment_ids.len() as u64 {
            error!(
                "Failed to link attachments: expected {}, got {}",
                attachment_ids.len(),
                rows_affected
            );
            // Unlink any that were linked
            sqlx::query(
                "UPDATE attachments SET dm_message_id = NULL WHERE dm_message_id = $1",
            )
            .bind(id)
            .execute(&state.db)
            .await
            .ok();
            Vec::new()
        } else {
            let att_rows = sqlx::query(
                "SELECT id, filename, mime_type, file_size FROM attachments WHERE id = ANY($1)",
            )
            .bind(&attachment_ids)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

            att_rows
                .into_iter()
                .map(|r| Attachment {
                    id: r.try_get("id").unwrap_or(0),
                    filename: r.try_get("filename").unwrap_or_default(),
                    mime_type: r.try_get("mime_type").unwrap_or_default(),
                    file_size: r.try_get("file_size").unwrap_or(0),
                })
                .collect()
        }
    } else {
        Vec::new()
    };

    let dm_msg = DirectMessage {
        id,
        sender_beam: claims.sub.clone(),
        recipient_beam: to.clone(),
        content,
        created_at,
        attachments,
    };

    if let Ok(subscribers) = state.dm_subscribers.lock()
        && let Some(tx) = subscribers.get(&to).cloned()
    {
        let _ = tx.send(json!(&dm_msg).to_string());
    }

    Ok(Json(json!({ "ok": true, "id": id })))
}

// GET /dm/ws — WebSocket for real-time DM delivery
pub async fn dm_ws(
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let claims = match extract_token(&*state.signing_key, &headers).await {
        Ok(claims) => Ok(claims),
        Err(_) => {
            if let Some(token) = params.get("token") {
                match decode_access_token(token, &*state.signing_key) {
                    Ok(claims) => Ok(claims),
                    Err(_) => Err((
                        StatusCode::UNAUTHORIZED,
                        Json(ErrorResponse {
                            error: "Invalid or expired token".into(),
                        }),
                    )),
                }
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse {
                        error: "Unauthorized".into(),
                    }),
                ))
            }
        }
    };

    match claims {
        Ok(claims) => {
            let beam = claims.sub.clone();
            let sender_id = claims.uid.clone();
            let tx = {
                let mut subscribers = state.dm_subscribers.lock().map_err(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse {
                            error: "Internal error".into(),
                        }),
                    )
                })?;
                let tx = subscribers.entry(beam.clone()).or_insert_with(|| {
                    let (tx, _) = broadcast::channel(100);
                    tx
                });
                tx.clone()
            };
            Ok(ws.on_upgrade(move |socket| {
                handle_dm_websocket(socket, state, beam, sender_id, tx)
            }))
        }
        Err(e) => Err(e),
    }
}

async fn handle_dm_websocket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    beam: String,
    sender_id: String,
    tx: broadcast::Sender<String>,
) {
    let _ = socket
        .send(Message::Text(
            json!({ "type": "system", "msg": "connected" }).to_string(),
        ))
        .await;

    let (mut sender, mut receiver) = socket.split();
    let mut rx = tx.subscribe();
    let beam_clone = beam.clone();

    loop {
        tokio::select! {
            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(text) => {
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            maybe_msg = receiver.next() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(req) = serde_json::from_str::<SendDmRequest>(&text) {
                            let content = req.content.trim().to_string();
                            if content.is_empty() {
                                let _ = sender.send(Message::Text(json!({ "type": "error", "msg": "Empty message" }).to_string())).await;
                                continue;
                            }

                            let recipient_id = match get_user_id_by_beam(&state.db, &req.to).await {
                                Some(uid) => uid,
                                None => {
                                    error!("Recipient user not found for beam: {}", &req.to);
                                    let _ = sender.send(Message::Text(json!({ "type": "error", "msg": "Recipient not found" }).to_string())).await;
                                    return;
                                }
                            };

                            let id_result: Result<i64, _> = sqlx::query_scalar(
                                "INSERT INTO dm_messages (sender_beam, recipient_beam, sender_id, recipient_id, content, created_at)
                                 VALUES ($1, $2, $3, $4, $5, NOW()::TEXT)
                                 RETURNING id",
                            )
                            .bind(&beam_clone)
                            .bind(&req.to)
                            .bind(&sender_id)
                            .bind(&recipient_id)
                            .bind(&content)
                            .fetch_one(&state.db)
                            .await;

                            let id = match id_result {
                                Ok(id) => id,
                                Err(e) => {
                                    error!("Failed to insert DM message: {}", e);
                                    return;
                                }
                            };

                            let created_at: String = sqlx::query_scalar(
                                "SELECT created_at FROM dm_messages WHERE id = $1",
                            )
                            .bind(id)
                            .fetch_one(&state.db)
                            .await
                            .unwrap_or_default();

                            let attachments = if !req.attachment_ids.is_empty() {
                                let count: i64 = sqlx::query_scalar(
                                    "SELECT COUNT(*) FROM attachments WHERE id = ANY($1) AND uploaded_by = $2 AND dm_message_id IS NULL",
                                )
                                .bind(&req.attachment_ids)
                                .bind(&beam_clone)
                                .fetch_one(&state.db)
                                .await
                                .unwrap_or(0);

                                if count != req.attachment_ids.len() as i64 {
                                    error!("Attachment count mismatch: expected {}, got {}", req.attachment_ids.len(), count);
                                    Vec::new()
                                } else {
                                    let update_result = sqlx::query(
                                        "UPDATE attachments SET dm_message_id = $1 WHERE id = ANY($2) AND dm_message_id IS NULL",
                                    )
                                    .bind(id)
                                    .bind(&req.attachment_ids)
                                    .execute(&state.db)
                                    .await;

                                    if let Err(e) = update_result {
                                        error!("Failed to update attachments: {}", e);
                                        return;
                                    }

                                    let att_rows = sqlx::query(
                                        "SELECT id, filename, mime_type, file_size FROM attachments WHERE id = ANY($1)",
                                    )
                                    .bind(&req.attachment_ids)
                                    .fetch_all(&state.db)
                                    .await
                                    .unwrap_or_default();

                                    att_rows
                                        .into_iter()
                                        .map(|r| Attachment {
                                            id: r.try_get("id").unwrap_or(0),
                                            filename: r.try_get("filename").unwrap_or_default(),
                                            mime_type: r.try_get("mime_type").unwrap_or_default(),
                                            file_size: r.try_get("file_size").unwrap_or(0),
                                        })
                                        .collect()
                                }
                            } else {
                                Vec::new()
                            };

                            let dm_msg = DirectMessage {
                                id,
                                sender_beam: beam_clone.clone(),
                                recipient_beam: req.to.clone(),
                                content,
                                created_at,
                                attachments,
                            };

                            if let Ok(subscribers) = state.dm_subscribers.lock()
                                && let Some(tx2) = subscribers.get(&req.to).cloned()
                            {
                                let _ = tx2.send(json!(&dm_msg).to_string());
                            }

                            let _ = sender.send(Message::Text(json!({ "type": "sent", "id": id }).to_string())).await;
                        }
                    }
                    _ => break,
                }
            }
        }
    }
}

// POST /upload
pub async fn upload_file_auth(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let claims = match extract_token(&*state.signing_key, &headers).await {
        Ok(claims) => claims,
        Err(e) => return e.into_response(),
    };

    let mut attachments_data = Vec::new();
    let mut total_size: i64 = 0;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let filename: String = match field.file_name() {
            Some(name) => sanitize_filename(name).to_string(),
            None => continue,
        };

        let mime_type: String = match field.content_type() {
            Some(mime) => mime.to_string(),
            None => continue,
        };

        if !ALLOWED_MIME_TYPES.contains(&mime_type.as_str()) {
            continue;
        }

        let mut bytes = Vec::new();
        while let Ok(Some(chunk)) = field.chunk().await {
            total_size += chunk.len() as i64;
            if total_size > MAX_FILE_SIZE {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "File too large. Maximum size is 10MB." })),
                )
                    .into_response();
            }
            bytes.extend_from_slice(&chunk);
        }

        if bytes.is_empty() {
            continue;
        }

        attachments_data.push((filename, mime_type, bytes));
    }

    if attachments_data.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "No valid files uploaded" })),
        )
            .into_response();
    }

    if total_size > MAX_FILE_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "Total file size exceeds 10MB limit." })),
        )
            .into_response();
    }

    let mut response_attachments = Vec::new();

    for (filename, mime_type, data) in attachments_data {
        let file_size = data.len() as i64;
        let attachment_id: Result<i64, _> = sqlx::query_scalar(
            "INSERT INTO attachments (filename, mime_type, file_size, file_data, uploaded_by)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(&filename)
        .bind(&mime_type)
        .bind(file_size)
        .bind(&data)
        .bind(&claims.sub)
        .fetch_one(&state.db)
        .await;

        match attachment_id {
            Ok(id) => {
                response_attachments.push(AttachmentUploadResponse {
                    attachment_id: id,
                    filename: filename.clone(),
                    mime_type: mime_type.clone(),
                    file_size,
                });
            }
            Err(e) => {
                error!("insert attachment: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to store file" })),
                )
                    .into_response();
            }
        }
    }

    Json(UploadResult {
        ok: true,
        attachments: response_attachments,
    })
    .into_response()
}

// GET /attachments/:id
pub async fn get_attachment_auth(
    headers: HeaderMap,
    Path(attachment_id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let claims = if let Ok(id) = extract_token(&*state.signing_key, &headers).await {
        Ok(id)
    } else if let Some(token) = params.get("token") {
        decode_access_token(token, &*state.signing_key).map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid or expired token".into(),
                }),
            )
        })
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Missing Authorization header".into(),
            }),
        ))
    };

    let _claims = match claims {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let row = sqlx::query(
        "SELECT filename, mime_type, file_size, file_data FROM attachments WHERE id = $1",
    )
    .bind(attachment_id)
    .fetch_optional(&state.db)
    .await;

    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Attachment not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("query attachment: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Database error" })),
            )
                .into_response();
        }
    };

    let filename: String = row.try_get("filename").unwrap_or_default();
    let mime_type: String = row.try_get("mime_type").unwrap_or_default();
    let data: Vec<u8> = row.try_get("file_data").unwrap_or_default();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        axum::http::header::CONTENT_TYPE,
        mime_type
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    let disposition = if mime_type.starts_with("image/") {
        format!("inline; filename=\"{}\"", filename)
    } else {
        format!("attachment; filename=\"{}\"", filename)
    };
    response_headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        disposition
            .parse()
            .unwrap_or_else(|_| "attachment".parse().unwrap()),
    );

    (StatusCode::OK, response_headers, data).into_response()
}
