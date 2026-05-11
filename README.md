# Zpulse — Direct Messaging Service

**Zpulse** handles one-on-one direct messages (DMs) between users in the Zeeble platform. It provides real-time WebSocket communication, presence via Redis, file attachments with authentication, and voice integration through LiveKit.

## Architecture

- **Language**: Rust + Axum + Tokio
- **Database**: PostgreSQL for persistent message storage
- **Pub/Sub**: Redis for online presence and WebSocket message broadcasting
- **Authentication**: JWT tokens validated against Zbeam (Auth service)
- **Real-time**: WebSocket connections per server for each user

```
Client ↔ Zpulse (WebSocket)
       ↓
   Redis (presence, pub/sub)
       ↓
   PostgreSQL (message history)
       ↓
   Zbeam (JWT validation)
```

### Key Components

- `src/handlers/dms.rs` — DM sending, listing, conversation management
- `src/handlers/livekit.rs` — LiveKit voice token generation for DMs
- `src/handlers/websocket.rs` — WebSocket connection handling, message broadcast
- `src/websocket.rs` — Low-level WebSocket protocol (ping/pong, auth)
- `src/models.rs` — Database models (users, servers, channels, messages)

## Quick Start

### Prerequisites

- Docker + Docker Compose (recommended)
- PostgreSQL
- Redis
- Zbeam (Auth service) running on `http://localhost:8001` (or set `ZBEAM_URL`)

### Using Docker Compose

```bash
cd DM-server
# Create .env if needed (see Environment Variables below)
docker compose up -d
```

This starts:
- **PostgreSQL** on port `5432` (container name `zpulse-postgres`)
- **Redis** on port `6379` (container name `zpulse-redis`)
- **Zpulse** on port `3002` (HTTP + WebSocket)

### Running from Source

```bash
cd DM-server
# Set environment variables (DATABASE_URL, ZBEAM_URL, REDIS_URL, PORT)
cargo run --release
```

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | Yes | — | PostgreSQL connection string (e.g., `postgresql://user:pass@localhost:5432/zpulse_db`) |
| `ZBEAM_URL` | No | `http://localhost:8001` | Base URL of the Auth service for JWT validation and fetching public keys. |
| `REDIS_URL` | No | `redis://localhost:6379` | Redis connection URL. |
| `PORT` | No | `3002` | Port to bind the HTTP server. |
| `RUST_LOG` | No | `zpulse=info,tower_http=debug` | Log filter (tracing-subscriber). |

### Docker Compose Override

You can provide a `.env` file in the `DM-server` directory. The provided `docker-compose.yml` uses:
- PostgreSQL image `postgres:15-alpine`
- Redis image `redis:7-alpine`
- Custom Dockerfile for Zpulse

## API Endpoints

All endpoints (except `/health`) require `Authorization: Bearer <jwt>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/servers` | List servers the user is a member of |
| `GET` | `/dms` | List DM conversations (with latest message, unread count) |
| `GET` | `/dms/:user_id` | Get DM conversation with a specific user |
| `POST` | `/dms/:user_id/send` | Send a DM (JSON: `{ "content": "string", "attachments?: []` }) |
| `POST` | `/upload` | Upload file attachment (multipart/form-data). Requires `X-Zpulse-Auth: <jwt>` header (separate from normal auth to allow upload without full DM context). |
| `GET` | `/attachments/:id` | Download attachment (no auth required if link is valid; embed token in URL) |

### WebSocket

Connect to: `ws://localhost:3002/ws?server_id=<server_uuid>`

**Authentication**: Immediately upon connection, send a JSON frame:

```json
{
  "type": "auth",
  "token": "<access_jwt>"
}
```

If valid, the server will respond with an `auth_ok` message and begin streaming DM events for that server.

**Client → Server messages**:

```json
{ "type": "send_dm", "to_user_id": "uuid", "content": "Hello", "attachments": [] }
{ "type": "read_receipt", "message_id": "uuid" }
{ "type": "typing_start", "to_user_id": "uuid" }
{ "type": "typing_stop", "to_user_id": "uuid" }
{ "type": "ping" }
```

**Server → Client messages**:

```json
{ "type": "message", "id": "uuid", "from_user_id": "uuid", "content": "Hi", "attachments": [], "sent_at": "2025-05-11T..." }
{ "type": "message_deleted", "message_id": "uuid" }
{ "type": "typing", "from_user_id": "uuid", "is_typing": true }
{ "type": "presence", "user_id": "uuid", "status": "online|idle|dnd|offline" }
{ "type": "pong" }
```

## Database Schema

Migrations are in `src/migrations/`. Core tables:

### `users`

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID PRIMARY KEY | User identifier |
| `beam_identity` | TEXT UNIQUE | Beam identity (from Auth) |
| `display_name` | TEXT | Friendly name |
| `avatar_url` | TEXT | Optional avatar attachment URL |
| `status` | TEXT | `online`, `idle`, `dnd`, `offline` |
| `last_seen_at` | TIMESTAMPTZ | Last activity timestamp |

### `servers`

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID PRIMARY KEY | Server identifier (from Cloud or local) |
| `name` | VARCHAR(255) | Server name |
| `owner_id` | UUID | Owner's user ID |
| `icon_url` | TEXT | Optional server icon |
| `created_at` | TIMESTAMPTZ | Creation timestamp |

### `server_members`

| Column | Type | Description |
|--------|------|-------------|
| `server_id` | UUID REFERENCES servers(id) ON DELETE CASCADE | |
| `user_id` | UUID REFERENCES users(id) ON DELETE CASCADE | |
| `nickname` | VARCHAR(255) | Per-server nickname |
| `role` | VARCHAR(50) | `owner`, `admin`, `member` |
| `joined_at` | TIMESTAMPTZ | Membership timestamp |
| **Primary Key** | `(server_id, user_id)` | |

### `channel_messages`

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID PRIMARY KEY | Message ID |
| `channel_id` | UUID REFERENCES channels(id) ON DELETE CASCADE | Text channel this message belongs to |
| `author_id` | UUID REFERENCES users(id) | Sender |
| `content` | TEXT | Message text |
| `message_type` | VARCHAR(50) | `text`, `image`, `file`, etc. |
| `attachments` | JSONB | Array of attachment metadata `{id, filename, mime_type, size}` |
| `reply_to` | UUID REFERENCES channel_messages(id) | Parent message for threads |
| `created_at` | TIMESTAMPTZ | Sent timestamp |
| `updated_at` | TIMESTAMPTZ | Edit timestamp |
| `deleted_at` | TIMESTAMPTZ | Soft delete timestamp |

### `channels`

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID PRIMARY KEY | |
| `server_id` | UUID REFERENCES servers(id) ON DELETE CASCADE | |
| `name` | VARCHAR(255) | Channel name (e.g., `general`) |
| `channel_type` | VARCHAR(50) | `text`, `voice`, `arena` |
| `category_id` | UUID REFERENCES categories(id) | Optional category |
| `position` | INTEGER | Sort order within category |
| `topic` | TEXT | Channel description |
| `created_at` | TIMESTAMPTZ | |

### Other tables

- `categories` — channel categories
- `channel_members` — user membership in channels (for user-created DMs)
- `invite_links` — channel/server invites

## Presence & Redis

Redis is used for:
- **Online status**: keys `presence:{user_id}` with TTL (updated on WebSocket ping/pong)
- **Message fan-out**: When a DM is sent, publish to Redis channel `dm:{server_id}:{channel_id}`; all WebSocket workers subscribed to that channel broadcast to connected clients.
- **Typing indicators**: ephemeral Redis keys `typing:{server_id}:{channel_id}:{user_id}` with short TTL (5s).

## Voice Integration

Zpulse generates LiveKit access tokens for voice channels when requested:

```
GET /v1/voice/token?room=<room_name>
```

Uses `LIVEKIT_API_KEY` and `LIVEKIT_API_SECRET` (configured in the parent server's `.env`). The token includes the user's identity and room metadata.

## File Attachments

Uploads are handled via `POST /upload` (multipart). The response includes an attachment ID that can be included in DM messages. Files can be stored:
- As BLOBs in PostgreSQL (`attachments.file_data`)
- Or on disk if `ATTACHMENTS_DIR` is configured (recommended for large files)

See Server settings for max upload size (default 8MB).

## Security Considerations

- **JWT Validation**: All WebSocket connections must authenticate immediately. Invalid tokens disconnect.
- **Rate Limiting**: Auth endpoints rate-limited per IP; consider extending to DM send.
- **CORS**: Configure `ALLOWED_ORIGINS` to match your frontend.
- **TLS**: In production, terminate TLS at a reverse proxy (Caddy, Nginx) and set `X-Forwarded-Proto: https`. The service itself can run HTTP internally.
- **Database**: Use strong PostgreSQL passwords; limit DB access to internal network.

## Deployment Notes

- For horizontal scaling, run multiple instances behind a load balancer. Redis pub/sub ensures messages reach all WebSocket workers. **Sticky sessions are not required** if using Redis correctly.
- Ensure Redis is reachable by all instances.
- Set `REDIS_URL` to a managed Redis or a replicated setup for HA.
- PostgreSQL should be backed up regularly.
- Consider environment-specific `.env` files for staging/production.

## Monitoring

- Health endpoint: `/health` returns `"ok"` when the service is ready.
- Logs: use `RUST_LOG=zpulse=debug` for verbose output.
- Metrics: Basic counting via `tower-http::trace`. Consider adding Prometheus metrics.

## Testing

No automated tests yet. To test manually:

1. Start the service and dependencies.
2. Obtain a JWT from Zbeam (`/login`).
3. Connect WebSocket: `ws://localhost:3002/ws?server_id=<uuid>` and send `auth` message.
4. Send a DM: `POST /dms/:user_id/send` with auth header.
5. Observe the message arrive via WebSocket on the recipient's connection.

## License

MIT
