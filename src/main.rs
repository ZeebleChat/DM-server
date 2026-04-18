mod auth_helpers;
mod channels;
mod handlers;
mod messages;
mod models;
mod voice;
mod websocket;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};

use axum::{
    Json,
    Router,
    extract::{State, WebSocketUpgrade},
    http::{Method, header},
    response::IntoResponse,
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use dotenvy::dotenv;
use ed25519_dalek::{SigningKey, pkcs8::DecodePrivateKey};
use redis::{Client, aio::ConnectionManager};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{RwLock, broadcast};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use handlers::dms::{dm_ws, get_attachment_auth, list_dms, send_dm, upload_file_auth};
use handlers::livekit::create_token as livekit_dm_token;

// ─── Shared State ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Connections {
    pub users: HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>,
    pub channels: HashMap<Uuid, HashSet<String>>,
    pub voice_rooms: HashMap<Uuid, HashSet<String>>,
}

pub struct AppState {
    pub db: PgPool,
    pub dm_subscribers: Mutex<HashMap<String, broadcast::Sender<String>>>,
    pub signing_key: Arc<SigningKey>,
    /// Base64url-encoded Ed25519 public key x-coordinate, derived from signing_key at startup.
    pub ed25519_x: String,
    pub connections: Arc<RwLock<Connections>>,
    pub redis: ConnectionManager,
    pub livekit_url: String,
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
}

// ─── Startup helpers ──────────────────────────────────────────────────────────

fn build_cors_layer() -> CorsLayer {
    let allow_origin = if let Ok(origins_str) = std::env::var("ALLOWED_ORIGINS") {
        let origins: Vec<axum::http::HeaderValue> = origins_str
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        AllowOrigin::list(origins)
    } else {
        AllowOrigin::predicate(|origin: &axum::http::HeaderValue, _| {
            let s = origin.to_str().unwrap_or("");
            s == "tauri://localhost"
                || s.starts_with("http://localhost")
                || s.starts_with("https://localhost")
                || s.starts_with("http://tauri.localhost")
                || s.starts_with("https://tauri.localhost")
        })
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

async fn connect_with_retry(database_url: &str) -> PgPool {
    let mut delay = std::time::Duration::from_secs(1);
    loop {
        match PgPoolOptions::new()
            .max_connections(20)
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(e) => {
                eprintln!("PostgreSQL not ready, retrying in {}s: {}", delay.as_secs(), e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(16));
            }
        }
    }
}

async fn init_database(pool: &PgPool) -> Result<(), sqlx::Error> {
    let stmts = [
        "CREATE TABLE IF NOT EXISTS servers (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name VARCHAR(255) NOT NULL,
            owner_id UUID NOT NULL,
            icon_url TEXT,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        )",
        "CREATE TABLE IF NOT EXISTS server_members (
            server_id UUID REFERENCES servers(id) ON DELETE CASCADE,
            user_id UUID NOT NULL,
            nickname VARCHAR(255),
            role VARCHAR(50) DEFAULT 'member',
            joined_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            PRIMARY KEY (server_id, user_id)
        )",
        "CREATE TABLE IF NOT EXISTS channels (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            server_id UUID REFERENCES servers(id) ON DELETE CASCADE,
            name VARCHAR(255) NOT NULL,
            channel_type VARCHAR(50) DEFAULT 'text',
            category_id UUID,
            position INTEGER DEFAULT 0,
            topic TEXT,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        )",
        "CREATE TABLE IF NOT EXISTS channel_members (
            channel_id UUID REFERENCES channels(id) ON DELETE CASCADE,
            user_id UUID NOT NULL,
            joined_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            PRIMARY KEY (channel_id, user_id)
        )",
        "CREATE TABLE IF NOT EXISTS channel_messages (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            channel_id UUID REFERENCES channels(id) ON DELETE CASCADE,
            author_id UUID NOT NULL,
            content TEXT NOT NULL,
            message_type VARCHAR(50) DEFAULT 'text',
            attachments JSONB,
            reply_to UUID,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            updated_at TIMESTAMP WITH TIME ZONE,
            deleted_at TIMESTAMP WITH TIME ZONE
        )",
        "CREATE TABLE IF NOT EXISTS invite_links (
            code VARCHAR(255) PRIMARY KEY,
            channel_id UUID REFERENCES channels(id) ON DELETE CASCADE,
            server_id UUID REFERENCES servers(id) ON DELETE CASCADE,
            created_by UUID NOT NULL,
            max_uses INTEGER,
            uses_count INTEGER DEFAULT 0,
            expires_at TIMESTAMP WITH TIME ZONE,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        )",
        "CREATE TABLE IF NOT EXISTS voice_rooms (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            channel_id UUID REFERENCES channels(id) ON DELETE CASCADE,
            name VARCHAR(255) NOT NULL,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        )",
        "CREATE TABLE IF NOT EXISTS voice_participants (
            room_id UUID REFERENCES voice_rooms(id) ON DELETE CASCADE,
            user_id UUID NOT NULL,
            joined_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            PRIMARY KEY (room_id, user_id)
        )",
        "CREATE INDEX IF NOT EXISTS idx_servers_owner ON servers(owner_id)",
        "CREATE INDEX IF NOT EXISTS idx_channels_server ON channels(server_id)",
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel ON channel_messages(channel_id)",
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_author ON channel_messages(author_id)",
        "CREATE INDEX IF NOT EXISTS idx_voice_rooms_channel ON voice_rooms(channel_id)",
    ];

    for sql in &stmts {
        sqlx::query(sql).execute(pool).await?;
    }
    Ok(())
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket::handle_connection(socket, state))
}

async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "zpulse",
        "timestamp": Utc::now().to_rfc3339()
    }))
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("zpulse=info".parse().unwrap())
        )
        .init();

    dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = connect_with_retry(&database_url).await;
    init_database(&pool).await.expect("Failed to initialize database schema");

    let key_path = std::env::var("JWT_PRIVATE_KEY_PATH")
        .unwrap_or_else(|_| "keys/auth-private.pem".to_string());
    let signing_key: Arc<SigningKey> = {
        let pem = fs::read_to_string(&key_path)
            .unwrap_or_else(|_| panic!("Cannot read Ed25519 key from {key_path}"));
        Arc::new(SigningKey::from_pkcs8_pem(&pem).expect("Failed to parse Ed25519 private key"))
    };

    let ed25519_x = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let redis_conn = Client::open(redis_url)
        .expect("Invalid REDIS_URL")
        .get_connection_manager()
        .await
        .expect("Failed to connect to Redis");

    let state = Arc::new(AppState {
        db: pool,
        dm_subscribers: Mutex::new(HashMap::new()),
        signing_key,
        ed25519_x,
        connections: Arc::new(RwLock::new(Connections::default())),
        redis: redis_conn,
        livekit_url: std::env::var("LIVEKIT_URL")
            .unwrap_or_else(|_| "http://localhost:7880".to_string()),
        livekit_api_key: std::env::var("LIVEKIT_API_KEY")
            .unwrap_or_else(|_| "zeeble-dev-key".to_string()),
        livekit_api_secret: std::env::var("LIVEKIT_API_SECRET")
            .unwrap_or_else(|_| "change-me-livekit-secret-min-32-chars".to_string()),
    });

    let app = Router::new()
        // Direct messages
        .route("/dms", get(list_dms).post(send_dm))
        .route("/dm/ws", get(dm_ws))
        .route("/upload", post(upload_file_auth))
        .route("/attachments/:id", get(get_attachment_auth))
        // Channel WebSocket
        .route("/ws", get(ws_handler))
        // Servers & channels
        .route("/servers", get(channels::list_servers).post(channels::create_server))
        .route("/servers/:id/channels", get(channels::get_server_channels).post(channels::create_server_channel))
        .route("/servers/:id/members", get(channels::get_server_members))
        .route("/channels", get(channels::list_channels).post(channels::create_channel))
        .route("/channels/:id", get(channels::get_channel).delete(channels::delete_channel))
        .route("/channels/:id/members", get(channels::get_channel_members))
        .route("/channels/:id/invite", post(channels::create_invite))
        // Channel messages
        .route("/messages/:channel_id", get(messages::get_messages).post(messages::send_message))
        .route("/messages/:channel_id/:message_id", delete(messages::delete_message))
        // Voice
        .route("/voice/join", post(voice::join_voice))
        .route("/voice/leave", post(voice::leave_voice))
        .route("/voice/rooms", get(voice::list_voice_rooms))
        .route("/voice/rooms/:id/participants", get(voice::get_participants))
        // LiveKit tokens — authenticated DM calls vs channel voice
        .route("/livekit/token", post(livekit_dm_token))
        .route("/voice/token", post(voice::get_livekit_token))
        // Health
        .route("/health", get(health_check))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(build_cors_layer())
        .layer(TraceLayer::new_for_http());

    let port = std::env::var("PORT").unwrap_or_else(|_| "3002".to_string());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Zpulse running on http://localhost:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("Failed to bind {addr}: {e}"));
    axum::serve(listener, app).await.expect("Server error");
}
