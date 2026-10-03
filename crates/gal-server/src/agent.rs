//! A deliberately small headless surface: one credential, one wavelet.
//!
//! Agent credentials are not browser sessions and cannot be used to manage
//! accounts or subscribe to the broader WebSocket surface.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{header, request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use gal_core::model::*;
use gal_core::protocol::ClientMessage;
use serde::{Deserialize, Serialize};

use crate::auth::{self, Identity};
use crate::http::{bad_request, not_found, server_error, Unauthorised};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentScope {
    Read,
    Reply,
}

impl AgentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Reply => "reply",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToken {
    pub id: String,
    pub user_id: UserId,
    pub wave_id: WaveId,
    pub wavelet_id: WaveletId,
    pub label: String,
    pub scope: AgentScope,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

pub struct AgentIdentity {
    pub token: AgentToken,
    pub token_hash: String,
}

impl FromRequestParts<Arc<AppState>> for AgentIdentity {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Response> {
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.is_empty())
            .map(|(_, token)| token)
            .ok_or_else(|| Unauthorised.into_response())?;
        let token_hash = auth::hash_token(bearer);
        let token = state
            .db
            .agent_token(token_hash.clone())
            .await
            .map_err(server_error)?
            .ok_or_else(|| Unauthorised.into_response())?;
        Ok(Self { token, token_hash })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateToken {
    wavelet_id: WaveletId,
    label: String,
    scope: AgentScope,
    /// Short credentials are useful for a single agent run. Never more than 30 days.
    expires_in_seconds: u32,
}

async fn create_token(
    State(state): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<CreateToken>,
) -> Response {
    let label = body.label.trim();
    if label.is_empty()
        || label.len() > 100
        || label.chars().any(char::is_control)
        || !valid_id(body.wavelet_id.as_str(), "s-")
        || body.expires_in_seconds == 0
        || body.expires_in_seconds > 30 * 24 * 60 * 60
    {
        return bad_request(
            "Use a label of 1–100 bytes, a wavelet id, and an expiry of 1–2592000 seconds.",
        );
    }
    if !state.check_command_rate(
        &identity.user.id,
        &ClientMessage::Open {
            wave_id: WaveId::from(""),
        },
    ) {
        return rate_limited();
    }
    // Resolve the scope only from wavelets this account participates in.
    let Some(wave_id) = (match state
        .db
        .wave_for_participant(&identity.user.id, &body.wavelet_id)
        .await
    {
        Ok(id) => id,
        Err(e) => return server_error(e),
    }) else {
        return not_found("That wavelet is not available.");
    };
    let created_at = now();
    let metadata = AgentToken {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: identity.user.id,
        wave_id,
        wavelet_id: body.wavelet_id,
        label: label.to_string(),
        scope: body.scope,
        created_at,
        expires_at: created_at + i64::from(body.expires_in_seconds) * 1000,
    };
    let token = format!("gal_agent_{}", auth::generate_token());
    match state
        .db
        .create_agent_token(metadata.clone(), auth::hash_token(&token))
        .await
    {
        Ok(true) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "token": token, "credential": metadata })),
        )
            .into_response(),
        Ok(false) => not_found("That wavelet is not available."),
        Err(e) => server_error(e),
    }
}

async fn list_tokens(State(state): State<Arc<AppState>>, identity: Identity) -> Response {
    match state.db.agent_tokens(identity.user.id).await {
        Ok(tokens) => Json(serde_json::json!({ "credentials": tokens })).into_response(),
        Err(e) => server_error(e),
    }
}

async fn revoke_token(
    State(state): State<Arc<AppState>>,
    identity: Identity,
    Path(id): Path<String>,
) -> Response {
    match state.db.revoke_agent_token(identity.user.id, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("That credential is not available."),
        Err(e) => server_error(e),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBlip {
    pub id: BlipId,
    pub parent: Option<BlipId>,
    pub author: UserId,
    pub revision: u64,
    pub seq: i64,
    pub comment_id: Option<CommentId>,
    pub comment_resolved: Option<bool>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContext {
    pub wave_id: WaveId,
    pub wavelet_id: WaveletId,
    pub title: String,
    pub mode: String,
    pub blips: Vec<ContextBlip>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextQuery {
    after: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_text_units")]
    text_units: usize,
}
fn default_limit() -> usize {
    50
}
fn default_text_units() -> usize {
    16000
}

async fn context(
    State(state): State<Arc<AppState>>,
    identity: AgentIdentity,
    Query(query): Query<ContextQuery>,
) -> Response {
    if !(1..=100).contains(&query.limit) || !(2..=64000).contains(&query.text_units) {
        return bad_request("limit must be 1–100 and textUnits must be 2–64000.");
    }
    let after = match query.after {
        None => (-1, String::new()),
        Some(cursor) => match cursor.split_once(':') {
            Some((seq, id)) if valid_id(id, "b-") => match seq.parse::<i64>() {
                Ok(seq) if seq >= 0 => (seq, id.to_string()),
                _ => return bad_request("Invalid context cursor."),
            },
            _ => return bad_request("Invalid context cursor."),
        },
    };
    if !state.check_command_rate(
        &identity.token.user_id,
        &ClientMessage::Open {
            wave_id: identity.token.wave_id,
        },
    ) {
        return rate_limited();
    }
    match state
        .db
        .agent_context(identity.token_hash, after, query.limit, query.text_units)
        .await
    {
        Ok(Some(context)) => Json(context).into_response(),
        Ok(None) => Unauthorised.into_response(),
        Err(e) => server_error(e),
    }
}

pub fn rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "5")],
        Json(
            serde_json::json!({ "error": "Slow down and retry later.", "code": "tooManyRequests" }),
        ),
    )
        .into_response()
}

/// Preserve Unicode scalars while counting the same UTF-16 units as the editor.
pub fn bounded_text(text: &str, budget: usize) -> (String, usize, bool) {
    let mut units = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > budget {
            break;
        }
        units += ch.len_utf16();
        end = index + ch.len_utf8();
    }
    (text[..end].to_string(), units, end < text.len())
}

fn valid_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && id.len() <= 64 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/agent-tokens", get(list_tokens).post(create_token))
        .route("/api/agent-tokens/{id}", delete(revoke_token))
        .route("/api/agent/context", get(context))
        .layer(axum::middleware::from_fn(no_store))
}

async fn no_store(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
