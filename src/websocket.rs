use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::AppState;
use crate::auth_helpers::decode_access_token;
use crate::models::AccessClaims;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsMessage {
    #[serde(rename = "auth")]
    Auth { token: String },
    #[serde(rename = "auth_ok")]
    AuthOk { user_id: String, username: String },
    #[serde(rename = "auth_error")]
    AuthError { message: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "message")]
    Message {
        channel_id: String,
        message_id: String,
        author_id: String,
        content: String,
        timestamp: String,
    },
    #[serde(rename = "typing")]
    Typing { channel_id: String, user_id: String, username: String },
    #[serde(rename = "presence")]
    Presence { user_id: String, status: String },
    #[serde(rename = "voice_update")]
    VoiceUpdate { room_id: String, user_id: String, action: String },
    #[serde(rename = "member_update")]
    MemberUpdate { server_id: String, user_id: String, action: String },
    #[serde(rename = "channel_update")]
    ChannelUpdate { server_id: String, channel_id: String, action: String },
    #[serde(rename = "error")]
    Error { message: String },
}

pub async fn handle_connection(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut authenticated = false;
    let mut user_id: Option<String> = None;
    let mut username: Option<String> = None;

    while let Some(msg) = receiver.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                warn!("WebSocket error: {}", e);
                break;
            }
        };

        let Message::Text(text) = msg else { continue };

        match serde_json::from_str::<WsMessage>(&text) {
            Ok(ws_msg) => match ws_msg {
                WsMessage::Auth { token } => {
                    match decode_access_token(&token, &state.signing_key) {
                        Ok(claims) => {
                            authenticated = true;
                            let uid = claims.uid.clone();
                            let display = display_name_from_claims(&claims);
                            user_id = Some(uid.clone());
                            username = Some(display.clone());

                            let mut connections = state.connections.write().await;
                            connections.users.insert(uid.clone(), tx.clone());
                            drop(connections);

                            let _: Result<(), _> = state.redis.clone()
                                .set_ex(format!("presence:{}", uid), "online", 300u64)
                                .await;

                            let _ = tx.send(serde_json::to_string(&WsMessage::AuthOk {
                                user_id: uid,
                                username: display,
                            }).unwrap());

                            info!("User authenticated via WebSocket");
                        }
                        Err(_) => {
                            let _ = tx.send(serde_json::to_string(&WsMessage::AuthError {
                                message: "Invalid or expired token".to_string(),
                            }).unwrap());
                        }
                    }
                }
                WsMessage::Ping => {
                    let _ = tx.send(serde_json::to_string(&WsMessage::Pong).unwrap());
                }
                WsMessage::Typing { channel_id, .. } if authenticated => {
                    broadcast_to_channel(&state, &channel_id, &WsMessage::Typing {
                        channel_id: channel_id.clone(),
                        user_id: user_id.clone().unwrap_or_default(),
                        username: username.clone().unwrap_or_default(),
                    }, None).await;
                }
                WsMessage::Message { channel_id, message_id, author_id, content, .. } if authenticated => {
                    let msg = WsMessage::Message {
                        channel_id: channel_id.clone(),
                        message_id,
                        author_id,
                        content,
                        timestamp: Utc::now().to_rfc3339(),
                    };
                    broadcast_to_channel(&state, &channel_id, &msg, None).await;
                }
                WsMessage::VoiceUpdate { room_id, user_id: uid, action } if authenticated => {
                    handle_voice_update(&state, &room_id, &uid, &action).await;
                }
                _ => {}
            },
            Err(e) => {
                warn!("Failed to parse WebSocket message: {}", e);
            }
        }
    }

    if let Some(uid) = user_id {
        let mut connections = state.connections.write().await;
        connections.users.remove(&uid);
        drop(connections);
        let _: Result<(), _> = state.redis.clone()
            .set_ex(format!("presence:{}", uid), "offline", 300u64)
            .await;
    }
}

fn display_name_from_claims(claims: &AccessClaims) -> String {
    claims.sub.split('»').next().unwrap_or(&claims.sub).to_string()
}

async fn broadcast_to_channel(
    state: &AppState,
    channel_id: &str,
    message: &WsMessage,
    exclude_user: Option<&str>,
) {
    let Ok(cid) = Uuid::parse_str(channel_id) else { return };
    let connections = state.connections.read().await;

    if let Some(users) = connections.channels.get(&cid) {
        let text = serde_json::to_string(message).unwrap();
        for uid in users {
            if exclude_user == Some(uid.as_str()) {
                continue;
            }
            if let Some(tx) = connections.users.get(uid) {
                let _ = tx.send(text.clone());
            }
        }
    }
}

async fn handle_voice_update(state: &AppState, room_id: &str, user_id: &str, action: &str) {
    let Ok(rid) = Uuid::parse_str(room_id) else { return };
    let mut connections = state.connections.write().await;

    let room = connections.voice_rooms.entry(rid).or_default();
    match action {
        "join" => { room.insert(user_id.to_string()); }
        "leave" => { room.remove(user_id); }
        _ => {}
    }

    let message = WsMessage::VoiceUpdate {
        room_id: room_id.to_string(),
        user_id: user_id.to_string(),
        action: action.to_string(),
    };
    let text = serde_json::to_string(&message).unwrap();

    let room_users: Vec<String> = connections.voice_rooms
        .get(&rid)
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default();

    for uid in &room_users {
        if let Some(tx) = connections.users.get(uid) {
            let _ = tx.send(text.clone());
        }
    }

    info!("Voice update: {} {} {}", room_id, user_id, action);
}
