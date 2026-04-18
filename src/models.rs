use serde::{Deserialize, Serialize};

// ─── JWT Claims ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: String,
    pub uid: String,
    pub parent_uid: Option<String>,
    pub account_type: String,
    pub premium: bool,
    pub verified: bool,
    pub exp: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_attachment_id: Option<i64>,
}

// ─── Request Types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DmListQuery {
    pub with: String,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct SendDmRequest {
    pub to: String,
    pub content: String,
    #[serde(default)]
    pub attachment_ids: Vec<i64>,
}

// ─── Response Types ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize, Clone)]
pub struct Attachment {
    pub id: i64,
    pub filename: String,
    pub mime_type: String,
    pub file_size: i64,
}

#[derive(Serialize)]
pub struct DirectMessage {
    pub id: i64,
    pub sender_beam: String,
    pub recipient_beam: String,
    pub content: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Serialize)]
pub struct AttachmentUploadResponse {
    pub attachment_id: i64,
    pub filename: String,
    pub mime_type: String,
    pub file_size: i64,
}

#[derive(Serialize)]
pub struct UploadResult {
    pub ok: bool,
    pub attachments: Vec<AttachmentUploadResponse>,
}
