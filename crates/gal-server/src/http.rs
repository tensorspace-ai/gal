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

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: "Something went wrong.".into(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignupRequest {
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    email: String,
    password: String,
}

#[derive(Deserialize)]
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
        .create_user(name, display_name, body.email.trim().to_string(), hash)
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
        _ => bad_request("Incorrect username or password."),
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
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
    if body.new_password.chars().count() < 8 {
        return bad_request("New password must be at least 8 characters.");
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

async fn static_asset(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    match ASSETS.iter().find(|a| a.0 == path) {
        Some(Asset(_, content_type, body)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                // The client is versioned with the binary, so revalidate rather
                // than risk serving a stale script against a new server.
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *body,
        )
            .into_response(),
        // Unknown paths fall back to the app shell so client-side routes such
        // as /wave/<id> survive a page reload.
        None => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            ASSETS[0].2,
        )
            .into_response(),
    }
}

/// Headers applied to every response.
///
/// The CSP is the second line of defence for the client: everything is served
/// from this origin and there is no inline script, so `'self'` is enough and
/// nothing legitimate needs `unsafe-inline`. `frame-ancestors 'none'` is what
/// stops the confirmation dialogs (remove participant, delete message) from
/// being clickjacked.
fn security_headers(config: &crate::config::Config) -> Vec<(header::HeaderName, &'static str)> {
    let mut headers = vec![
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:;              connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
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
        .route("/api/me", get(me))
        .route("/api/users", get(users))
        .route("/api/lookup", get(lookup))
        .route("/api/server", get(server_info))
        // Before the fallback, so it is a real signal rather than the app shell.
        .route("/healthz", get(health))
        .route("/ws", get(crate::ws::handler))
        .fallback(static_asset);

    for (name, value) in security_headers(&state.config) {
        router = router.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            name,
            axum::http::HeaderValue::from_static(value),
        ));
    }
    router.with_state(state)
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
    fn every_asset_is_non_empty_and_uniquely_routed() {
        let mut paths = std::collections::HashSet::new();
        for Asset(path, content_type, body) in ASSETS {
            assert!(!body.is_empty(), "{path} is empty");
            assert!(!content_type.is_empty());
            assert!(paths.insert(*path), "{path} is routed twice");
        }
    }
}
