//! HTTP surface: authentication endpoints, the identity extractor, and the
//! embedded web client.
//!
//! The client is compiled into the binary, so deploying Gal is copying one
//! file. There is no build step and no asset pipeline.

use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::{header, request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use gal_core::model::PublicUser;
use serde::{Deserialize, Serialize};

use crate::auth::{self, Identity};
use crate::db::SESSION_TTL_MS;
use crate::state::AppState;

/// Rejection returned when a request carries no valid session.
pub struct Unauthorised;

impl IntoResponse for Unauthorised {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "Not signed in.".into(),
            }),
        )
            .into_response()
    }
}

/// Extracts the signed-in user, or rejects the request.
///
/// Implemented as an extractor so that every authenticated handler — including
/// the WebSocket upgrade — gets the check by construction rather than by
/// remembering to call it.
impl FromRequestParts<Arc<AppState>> for Identity {
    type Rejection = Unauthorised;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let cookies = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .ok_or(Unauthorised)?;
        let token = auth::token_from_cookies(cookies).ok_or(Unauthorised)?;
        let token_hash = auth::hash_token(&token);

        let user = state
            .db
            .user_for_session(token_hash.clone())
            .await
            .map_err(|_| Unauthorised)?
            .ok_or(Unauthorised)?;

        Ok(Identity { user, token_hash })
    }
}

/// The peer address, when the server was started with connect-info.
///
/// Infallible on purpose: rate limiting must not be able to reject a request
/// just because the address is unavailable.
pub struct ClientAddr(pub Option<std::net::IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientAddr(
            parts
                .extensions
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

pub(crate) fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

pub(crate) fn not_found(message: impl Into<String>) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

fn payload_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(ApiError {
            error: "That file is too large. The limit is 10 MB.".into(),
        }),
    )
        .into_response()
}

pub(crate) fn server_error(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: "Something went wrong.".into(),
        }),
    )
        .into_response()
}

/// No `email`. One used to be accepted here and written to the users table,
/// where nothing ever read it: the client has never sent one, there is no
/// verification, no password reset to use it for, and no way to delete it. That
/// is unverified personal data collected for no purpose — the worst kind to
/// hold. Any value stored by an older build is left alone rather than deleted
/// out from under an operator; the column is still there and can be cleared
/// with `UPDATE users SET email = ''`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct SignupRequest {
    name: String,
    #[serde(default)]
    display_name: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    name: String,
    password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    user: PublicUser,
}

async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ClientAddr(peer): ClientAddr,
    Json(body): Json<SignupRequest>,
) -> Response {
    if let Some(response) = state.check_auth_rate(&headers, peer) {
        return response;
    }
    if !state.config.open_registration {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: "Registration is closed on this server.".into(),
            }),
        )
            .into_response();
    }

    let (name, display_name) =
        match auth::validate_signup(&body.name, &body.display_name, &body.password) {
            Ok(v) => v,
            Err(e) => return bad_request(e.to_string()),
        };

    match state.db.user_by_name(&name).await {
        Ok(Some(_)) => return bad_request("That username is already taken."),
        Err(e) => return server_error(e),
        Ok(None) => {}
    }

    // Argon2 is deliberately expensive: ~19 MiB and tens of milliseconds of CPU.
    // Running it inline would park a reactor thread, so a handful of concurrent
    // signups could stall every other request on the server, WebSockets included.
    let password = body.password.clone();
    let hash = match tokio::task::spawn_blocking(move || auth::hash_password(&password)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return server_error(e),
        Err(e) => return server_error(anyhow::anyhow!("password hashing panicked: {e}")),
    };

    let user = match state
        .db
        .create_user(name, display_name, String::new(), hash)
        .await
    {
        Ok(u) => u,
        // A concurrent signup with the same name loses the unique constraint.
        Err(_) => return bad_request("That username is already taken."),
    };

    state.cache_user(user.public()).await;
    issue_session(&state, &user).await
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ClientAddr(peer): ClientAddr,
    Json(body): Json<LoginRequest>,
) -> Response {
    if let Some(response) = state.check_auth_rate(&headers, peer) {
        return response;
    }
    // The address limiter throttles one attacker and does nothing about many of
    // them guessing at one account. Checked before the hash, so a locked
    // account also costs no CPU — and worded like every other failure, so it
    // does not become an oracle for which usernames exist.
    if state.account_is_throttled(&body.name) {
        tracing::warn!(account = %body.name.trim(), "sign-in throttled after repeated failures");
        return bad_request("Incorrect username or password.");
    }

    let stored = match state.db.user_by_name(body.name.trim()).await {
        Ok(Some(u)) => Some(u),
        // Verify against a dummy hash anyway, so an unknown username costs the
        // same as a known one and response time reveals nothing.
        Ok(None) => None,
        Err(e) => return server_error(e),
    };

    let hash = stored
        .as_ref()
        .map(|u| u.password_hash.clone())
        .unwrap_or_else(|| DUMMY_HASH.to_string());
    let password = body.password.clone();
    let ok =
        match tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash)).await {
            Ok(ok) => ok,
            Err(e) => return server_error(anyhow::anyhow!("password verification panicked: {e}")),
        };

    match (ok, stored) {
        (true, Some(user)) => issue_session(&state, &user).await,
        _ => {
            // Charged on failure only, so signing in normally — however often —
            // never moves this bucket.
            state.note_failed_signin(&body.name);
            bad_request("Incorrect username or password.")
        }
    }
}

/// A valid Argon2 hash of a throwaway password, used to equalise login timing
/// between existing and non-existent accounts.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Yl5S5xJHRLPTPXQD0oPGxNzTfPFHKPYnUdRJXvJ7Hqo";

async fn issue_session(state: &Arc<AppState>, user: &gal_core::model::User) -> Response {
    let token = auth::generate_token();
    if let Err(e) = state
        .db
        .create_session(&user.id, auth::hash_token(&token))
        .await
    {
        return server_error(e);
    }

    let cookie = auth::session_cookie(&token, state.config.secure_cookies, SESSION_TTL_MS / 1000);
    let mut headers = HeaderMap::new();
    if let Ok(value) = cookie.parse() {
        headers.insert(header::SET_COOKIE, value);
    }
    (
        headers,
        Json(SessionResponse {
            user: user.public(),
        }),
    )
        .into_response()
}

async fn logout(State(state): State<Arc<AppState>>, identity: Identity) -> Response {
    if let Err(e) = state.db.delete_session(identity.token_hash).await {
        return server_error(e);
    }
    let mut headers = HeaderMap::new();
    if let Ok(value) = auth::clear_cookie(state.config.secure_cookies).parse() {
        headers.insert(header::SET_COOKIE, value);
    }
    (headers, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// End every other session this account has.
///
/// The only way to do this used to be changing your password, which is an odd
/// thing to have to do about a laptop left on a train, and gave no idea how
/// many sessions there were to begin with.
async fn sign_out_everywhere(State(state): State<Arc<AppState>>, identity: Identity) -> Response {
    match state
        .db
        .revoke_other_sessions(&identity.user.id, identity.token_hash.clone())
        .await
    {
        Ok(revoked) => {
            tracing::info!(user = %identity.user.id, revoked, "signed out other sessions");
            Json(serde_json::json!({ "revoked": revoked })).into_response()
        }
        Err(e) => server_error(e),
    }
}

/// How many sessions this account has, so the client can say what signing out
/// everywhere would actually do.
async fn session_info(State(state): State<Arc<AppState>>, identity: Identity) -> Response {
    match state.db.session_count(&identity.user.id).await {
        Ok(sessions) => Json(serde_json::json!({ "sessions": sessions })).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// Change your own password. Requires the current one, so a stolen session
/// cannot be used to lock the real owner out.
async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ClientAddr(peer): ClientAddr,
    identity: Identity,
    Json(body): Json<PasswordChange>,
) -> Response {
    if let Some(response) = state.check_auth_rate(&headers, peer) {
        return response;
    }
    // The same rules registration applies. They used to differ: registration
    // checked a minimum and a change checked its own, so the one path a person
    // uses to *improve* a password was the laxer of the two.
    if let Err(e) = auth::validate_password(&body.new_password, Some(&identity.user.name)) {
        return bad_request(e.to_string());
    }

    let stored = identity.user.password_hash.clone();
    let current = body.current_password.clone();
    let ok =
        match tokio::task::spawn_blocking(move || auth::verify_password(&current, &stored)).await {
            Ok(ok) => ok,
            Err(e) => return server_error(anyhow::anyhow!("password verification panicked: {e}")),
        };
    if !ok {
        return bad_request("Current password is incorrect.");
    }

    let new_password = body.new_password.clone();
    let hash = match tokio::task::spawn_blocking(move || auth::hash_password(&new_password)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return server_error(e),
        Err(e) => return server_error(anyhow::anyhow!("password hashing panicked: {e}")),
    };

    match state
        .db
        .change_password(&identity.user.id, hash, identity.token_hash.clone())
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => server_error(e),
    }
}

async fn me(identity: Identity) -> Response {
    Json(SessionResponse {
        user: identity.user.public(),
    })
    .into_response()
}

/// People the caller already shares a wave with.
///
/// Deliberately not the full member directory: returning every account let any
/// throwaway signup enumerate and then target every user on the server. To add
/// someone new you type their exact username, which `/api/lookup` resolves.
async fn users(State(state): State<Arc<AppState>>, identity: Identity) -> Response {
    match state.db.known_users(&identity.user.id).await {
        Ok(users) => Json(users).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookupQuery {
    name: String,
}

/// Resolve one exact username, for adding a participant.
///
/// Exact-match only, and rate limited: this is an existence oracle by nature, so
/// it must not be usable to walk the directory.
async fn lookup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ClientAddr(peer): ClientAddr,
    identity: Identity,
    axum::extract::Query(query): axum::extract::Query<LookupQuery>,
) -> Response {
    if let Some(response) = state.check_lookup_rate(&headers, peer) {
        return response;
    }
    let _ = identity;
    match state.db.user_by_name(query.name.trim()).await {
        Ok(Some(user)) => Json(serde_json::json!({ "user": user.public() })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "No such user.".into(),
            }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

// --- attachments --------------------------------------------------------

/// Largest single upload. Bytes live in the database, so this is also the
/// largest row Gal will ever write.
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

/// How much one account may upload per day.
const UPLOAD_QUOTA_BYTES: u64 = 200 * 1024 * 1024;
const UPLOAD_QUOTA_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Images the server is willing to serve back with their real content type.
///
/// Recognised by magic bytes rather than by what the uploader called the file.
/// Everything else — including SVG, which is a document that can carry script —
/// is served as an opaque download, so there is no path from "upload a file" to
/// "run script on this origin".
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && starts(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Reduce an uploaded filename to something safe to show and to echo back in a
/// header: no directory components, no control characters, no quotes.
fn clean_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(120)
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Percent-encode for the `filename*` form of `Content-Disposition`, which is
/// the only one that can carry a name that is not ASCII.
fn encode_filename(name: &str) -> String {
    let mut out = String::new();
    for byte in name.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadQuery {
    #[serde(default)]
    name: String,
}

/// Accept a file for one wavelet.
///
/// The body is the file itself rather than a multipart form: there is one field
/// and no form to speak of, and a raw body needs no parser between the network
/// and the bytes.
async fn upload_attachment(
    State(state): State<Arc<AppState>>,
    identity: Identity,
    axum::extract::Path(wavelet_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Response {
    let wavelet_id = gal_core::model::WaveletId(wavelet_id);

    // The membership check comes first: a stranger must not be able to learn
    // that a wavelet exists, or to spend the server's disk on one.
    match state
        .db
        .is_wavelet_participant(&identity.user.id, &wavelet_id)
        .await
    {
        Ok(true) => {}
        // Same answer for "no such wavelet" and "not yours", so this cannot be
        // used to probe for private replies.
        Ok(false) => return not_found("No such conversation."),
        Err(e) => return server_error(e),
    }

    if body.is_empty() {
        return bad_request("That file is empty.");
    }
    if body.len() > MAX_ATTACHMENT_BYTES {
        return payload_too_large();
    }

    // What the file *is*, never what it was called or what the client declared.
    // A name ending in .png proves nothing, and believing it is how an upload
    // endpoint turns into a way to serve HTML from your own origin.
    let mime = sniff_image(&body)
        .unwrap_or("application/octet-stream")
        .to_string();
    let name = clean_filename(&query.name);

    match state
        .db
        .create_attachment(
            wavelet_id,
            identity.user.id.clone(),
            name,
            mime,
            body.into(),
            // The allowance is applied inside the insert's own transaction
            // rather than checked here: a check on this side runs on a
            // different pooled connection from the write, so every concurrent
            // upload would read the same pre-upload total and all would pass.
            crate::db::UploadQuota {
                bytes: UPLOAD_QUOTA_BYTES,
                since: gal_core::model::now() - UPLOAD_QUOTA_WINDOW_MS,
            },
        )
        .await
    {
        Ok(Some(attachment)) => (StatusCode::CREATED, Json(attachment)).into_response(),
        Ok(None) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiError {
                error: "You have uploaded too much today. Try again tomorrow.".into(),
            }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

/// Serve an attachment to someone in the wavelet it belongs to.
async fn get_attachment(
    State(state): State<Arc<AppState>>,
    identity: Identity,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let id = gal_core::model::AttachmentId(id);
    let (attachment, bytes) = match state.db.attachment_for(&identity.user.id, &id).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_found("No such attachment."),
        Err(e) => return server_error(e),
    };

    // Images were identified by their own bytes on the way in, so they can be
    // rendered in place. Anything else is handed over as an opaque download,
    // which is what stops an uploaded page from ever being a page on this
    // origin.
    let how = if attachment.is_image() {
        "inline"
    } else {
        "attachment"
    };
    let disposition = format!(
        "{how}; filename=\"{}\"; filename*=UTF-8''{}",
        attachment.name,
        encode_filename(&attachment.name)
    );

    let mut response_headers = HeaderMap::new();
    if let Ok(value) = attachment.mime.parse() {
        response_headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = disposition.parse() {
        response_headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // Immutable: the id names these exact bytes and nothing ever rewrites them.
    if let Ok(value) = "private, max-age=31536000, immutable".parse() {
        response_headers.insert(header::CACHE_CONTROL, value);
    }
    (response_headers, bytes).into_response()
}

/// Liveness and readiness. Touches the database, so it fails when the server
/// cannot actually serve. Registered before the SPA fallback, which would
/// otherwise answer every probe path with 200 and hide real outages.
async fn health(State(state): State<Arc<AppState>>) -> Response {
    match state.db.user_count().await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

/// Whether sign-up is available, so the login screen can hide the form.
async fn server_info(State(state): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "openRegistration": state.config.open_registration,
        "version": env!("CARGO_PKG_VERSION"),
        // `null` when no provider is configured, which is the only thing that
        // decides whether the sign-in button is drawn. The client is never told
        // the issuer or the client id.
        "oidc": crate::oidc::provider_info(&state),
    }))
    .into_response()
}

// --- embedded client ----------------------------------------------------

/// One embedded asset: path, content type, and bytes.
struct Asset(&'static str, &'static str, &'static str);

const ASSETS: &[Asset] = &[
    Asset(
        "/",
        "text/html; charset=utf-8",
        include_str!("web/index.html"),
    ),
    Asset(
        "/index.html",
        "text/html; charset=utf-8",
        include_str!("web/index.html"),
    ),
    Asset(
        "/style.css",
        "text/css; charset=utf-8",
        include_str!("web/style.css"),
    ),
    Asset(
        "/ot.js",
        "text/javascript; charset=utf-8",
        include_str!("web/ot.js"),
    ),
    Asset(
        "/client.js",
        "text/javascript; charset=utf-8",
        include_str!("web/client.js"),
    ),
    Asset(
        "/editor.js",
        "text/javascript; charset=utf-8",
        include_str!("web/editor.js"),
    ),
    Asset(
        "/ui.js",
        "text/javascript; charset=utf-8",
        include_str!("web/ui.js"),
    ),
    Asset(
        "/app.js",
        "text/javascript; charset=utf-8",
        include_str!("web/app.js"),
    ),
];

/// Prometheus exposition, behind a bearer token.
///
/// Off unless `GAL_METRICS_TOKEN` is set. What this returns says how many people
/// are using a server and when, which is not something to publish by default on
/// the assumption that an operator wrote a proxy rule for it.
async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(expected) = state.config.metrics_token.as_deref() else {
        // Indistinguishable from any other unrouted path, so a scan cannot tell
        // whether the endpoint exists but is guarded.
        return not_found("No such endpoint.");
    };

    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !auth::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "Not authorised.".into(),
            }),
        )
            .into_response();
    }

    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
        .into_response()
}

/// One log line per request, and a request id carried on the response.
///
/// There was no access log by default and no way to tie a 500 to the request
/// that caused it: `server_error` logged the error alone, so two concurrent
/// failures were indistinguishable in the output.
///
/// The path is logged without its query string. `/api/lookup?name=alice` would
/// otherwise put usernames in the log of every deployment that turns logging up.
async fn observe(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    // An id from a proxy in front of us wins, so one request has one id across
    // every hop that logged it.
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 200 && v.is_ascii())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    let status = response.status();
    let millis = started.elapsed().as_secs_f64() * 1000.0;

    state.metrics.http_response(status.as_u16());
    tracing::info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        status = status.as_u16(),
        duration_ms = format!("{millis:.1}"),
        "request"
    );

    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Paths the client routes itself, which must survive a page reload.
///
/// `app.js` writes exactly two shapes into the address bar — `/` and
/// `/wave/<id>` — so those are the only ones the shell answers for.
fn is_client_route(path: &str) -> bool {
    path == "/"
        || path
            .strip_prefix("/wave/")
            .is_some_and(|id| !id.contains('/'))
}

async fn static_asset(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    if let Some(Asset(_, content_type, body)) = ASSETS.iter().find(|a| a.0 == path) {
        return (
            [
                (header::CONTENT_TYPE, *content_type),
                // The client is versioned with the binary, so revalidate rather
                // than risk serving a stale script against a new server.
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *body,
        )
            .into_response();
    }

    // The shell is served for the client's own routes, and *only* for those.
    // Answering every unrouted path with it meant the server had no 404 at all:
    // a mistyped endpoint came back as 200 and a page of HTML, so a monitoring
    // system scraping /metrics recorded a healthy scrape of nothing, and a
    // client calling /api/wavelet by mistake parsed markup as its error body.
    if is_client_route(path) {
        return (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            ASSETS[0].2,
        )
            .into_response();
    }

    not_found("No such endpoint.")
}

/// Headers applied to every response.
///
/// The CSP is the second line of defence for the client: everything is served
/// from this origin and there is no inline script, so `'self'` is enough and
/// nothing legitimate needs `unsafe-inline`. `frame-ancestors 'none'` is what
/// stops the confirmation dialogs (remove participant, delete message) from
/// being clickjacked.
///
/// `connect-src` is `'self'` and nothing else. It used to read `'self' ws:
/// wss:`, which looks like "allow the WebSocket" and is not: in a source list
/// `ws:` and `wss:` are *scheme* sources, and a scheme source matches every
/// host. That turned the one directive standing between a script and the
/// network into a permit for streaming a participant's inbox anywhere on the
/// internet, which is most of what an XSS would want. `'self'` covers the
/// client's actual socket — CSP resolves it against the document's origin
/// including its `ws:`/`wss:` variant, and `/ws` is same-origin by
/// construction, since the client derives the URL from `location` and the
/// server enforces that with its own `Origin` check on the upgrade.
fn security_headers(config: &crate::config::Config) -> Vec<(header::HeaderName, &'static str)> {
    let mut headers = vec![
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "same-origin"),
    ];
    if config.hsts {
        headers.push((
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000; includeSubDomains",
        ));
    }
    headers
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .route("/api/sessions", get(session_info))
        .route("/api/sessions/revoke", post(sign_out_everywhere))
        .route("/api/me", get(me))
        .route("/api/oauth/start", get(crate::oidc::start))
        .route("/api/oauth/callback", get(crate::oidc::callback))
        .route("/api/users", get(users))
        .route("/api/lookup", get(lookup))
        .route("/api/server", get(server_info))
        .route(
            "/api/wavelets/{wavelet_id}/attachments",
            // The default body limit is 2 MB, which would reject most photographs
            // long before the handler's own check could explain why.
            post(upload_attachment).layer(axum::extract::DefaultBodyLimit::max(
                MAX_ATTACHMENT_BYTES + 1024,
            )),
        )
        .route("/api/attachments/{id}", get(get_attachment))
        // Before the fallback, so it is a real signal rather than the app shell.
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .route("/ws", get(crate::ws::handler))
        .fallback(static_asset);

    for (name, value) in security_headers(&state.config) {
        router = router.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            name,
            axum::http::HeaderValue::from_static(value),
        ));
    }
    router
        .layer(axum::middleware::from_fn_with_state(state.clone(), observe))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_hash_is_valid_so_timing_equalisation_actually_runs() {
        // If this stopped parsing, verify_password would return early and the
        // unknown-user path would get measurably faster than the known-user one.
        assert!(argon2::password_hash::PasswordHash::new(DUMMY_HASH).is_ok());
        assert!(!auth::verify_password("anything", DUMMY_HASH));
    }

    #[test]
    fn an_image_is_recognised_by_its_bytes_and_nothing_else() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));

        // The whole point: a name proves nothing. Both of these would be served
        // inline as HTML by anything that trusted the extension or the
        // uploader's Content-Type, and both would then run script on this
        // origin.
        assert_eq!(sniff_image(b"<script>alert(1)</script>"), None);
        assert_eq!(
            sniff_image(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"),
            None
        );
        assert_eq!(sniff_image(b""), None);
        assert_eq!(sniff_image(b"RIFF"), None, "a short prefix is not a match");
    }

    #[test]
    fn filenames_are_stripped_of_paths_and_anything_a_header_cannot_carry() {
        assert_eq!(clean_filename("plan.png"), "plan.png");
        assert_eq!(clean_filename("../../etc/passwd"), "passwd");
        assert_eq!(clean_filename("C:\\Users\\me\\notes.txt"), "notes.txt");
        // A quote or a newline here would end the Content-Disposition value
        // early and let the rest be read as further header content.
        assert_eq!(clean_filename("a\"b\nc.txt"), "abc.txt");
        assert_eq!(clean_filename("   "), "file");
        assert_eq!(clean_filename(""), "file");
        assert_eq!(clean_filename("...."), "file");
        assert!(clean_filename(&"x".repeat(500)).chars().count() <= 120);
    }

    #[test]
    fn non_ascii_filenames_survive_as_the_encoded_form() {
        assert_eq!(encode_filename("plan.png"), "plan.png");
        assert_eq!(encode_filename("a b"), "a%20b");
        assert_eq!(encode_filename("é"), "%C3%A9");
    }

    #[test]
    fn every_asset_is_non_empty_and_uniquely_routed() {
        let mut paths = std::collections::HashSet::new();
        for Asset(path, content_type, body) in ASSETS {
            assert!(!body.is_empty(), "{path} is empty");
            assert!(!content_type.is_empty());
            assert!(paths.insert(*path), "{path} is routed twice");
        }
    }

    #[test]
    fn the_shell_answers_for_client_routes_and_nothing_else() {
        // Reloading a deep link has to work: these are the paths app.js writes.
        assert!(is_client_route("/"));
        assert!(is_client_route("/wave/w-1"));

        // Everything else is a 404, so the server has one at all.
        assert!(!is_client_route("/metrics"));
        assert!(!is_client_route("/api/wavelet"));
        assert!(!is_client_route("/api/attachments"));
        assert!(!is_client_route("/wave"));
        assert!(!is_client_route("/wave/w-1/extra"));
        assert!(!is_client_route("/favicon.ico"));
    }

    /// A bare scheme in a CSP source list matches *every host* using that
    /// scheme, so `ws:` written to mean "our own WebSocket" silently permits a
    /// socket to anywhere. That is an exfiltration channel wearing the costume
    /// of a connectivity fix, and it is an easy one to reintroduce.
    #[test]
    fn the_csp_never_widens_a_directive_to_a_whole_scheme() {
        let config = crate::config::Config::default();
        let headers = security_headers(&config);
        let (_, csp) = headers
            .iter()
            .find(|(name, _)| name == header::CONTENT_SECURITY_POLICY)
            .expect("a CSP is sent on every response");

        for directive in csp.split(';').map(str::trim) {
            let (name, sources) = directive.split_once(' ').unwrap_or((directive, ""));
            for source in sources.split_whitespace() {
                // `data:` in img-src is deliberate: pasted screenshots render
                // from a data URI before the upload completes. It carries no
                // host, so it grants no reach off this origin.
                if source == "data:" && name == "img-src" {
                    continue;
                }
                assert!(
                    !source.ends_with(':'),
                    "{name} allows the whole {source} scheme, which matches every host"
                );
            }
        }
        assert!(
            csp.contains("connect-src 'self';"),
            "connect-src must stay pinned to this origin: {csp}"
        );
    }
}
