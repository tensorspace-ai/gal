//! Signing in through an external OpenID Connect provider.
//!
//! An authorization code flow with PKCE. Two endpoints: `/api/oauth/start`
//! sends the browser to the provider, `/api/oauth/callback` receives it back,
//! turns the code into a subject, and issues an ordinary session cookie.
//! Nothing downstream knows the difference — the `Identity` extractor, the
//! WebSocket upgrade and logout are all unchanged, because what the flow
//! produces is a row in `sessions` like any other.
//!
//! # The ID token's signature is deliberately not checked
//!
//! The provider returns an ID token signed with RS256, or ES256, or one of a
//! dozen others it may pick. Verifying it means fetching a JWKS, caching it,
//! handling key rotation and adding an RSA implementation — a meaningful amount
//! of new security-critical code.
//!
//! It is also unnecessary here. The code is exchanged over a direct
//! server-to-server TLS connection to the token endpoint, and the subject is
//! read from the userinfo endpoint over another. OIDC Core §3.1.3.7 says
//! exactly this: when the ID token is received by direct communication with the
//! token endpoint, TLS server authentication may be used to validate the issuer
//! in place of checking the token's signature. The ID token is therefore never
//! parsed at all, and the trust rests on one mechanism the flow already depends
//! on rather than two.
//!
//! The cost is one extra round trip per sign-in, and a sign-in is not a hot
//! path. It is also why [`crate::config`] refuses plain `http` to anything but
//! a loopback address: without TLS the argument above evaporates.
//!
//! # Why no `nonce` is sent
//!
//! A nonce binds an ID token to the request that asked for it. Since no ID
//! token is consumed there would be nothing to check it against, so sending one
//! would be decoration. What protects the round trip is PKCE, which ties the
//! code to this server, and the `state`/cookie pair, which ties it to this
//! browser.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use dashmap::DashMap;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth;
use crate::config::OidcConfig;
use crate::db::{OauthLogin, SESSION_TTL_MS};
use crate::state::AppState;

/// Name of the cookie holding the in-flight sign-in's secret.
const FLOW_COOKIE: &str = "gal_oauth";

/// How long a started sign-in stays valid.
///
/// Long enough to type a password and answer a second factor at the provider,
/// short enough that an abandoned flow is not a lasting entry in the map.
const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

/// Ceiling on flows in flight, after expired ones have been pruned.
///
/// `/api/oauth/start` is unauthenticated by necessity, so without a cap it is
/// an invitation to allocate memory a few hundred bytes at a time.
const MAX_FLOWS: usize = 4096;

/// How long a discovery document is reused before being fetched again.
const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);

/// A sign-in that has been started and not yet completed.
struct Pending {
    /// The `state` handed to the provider, to be compared with what comes back.
    state: String,
    /// The PKCE code verifier, whose challenge went out with the request.
    verifier: String,
    started: Instant,
}

/// The endpoints read out of the provider's discovery document.
#[derive(Clone, Debug, Deserialize)]
pub struct Endpoints {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
}

/// A configured provider, with its caches and its HTTP client.
pub struct Oidc {
    pub config: OidcConfig,
    /// Held rather than built per request: it is the connection pool, and the
    /// TLS session cache with it.
    http: reqwest::Client,
    /// Discovery is lazy and cached rather than done at startup: a provider
    /// that is briefly unreachable should not stop this server booting and
    /// serving everyone whose session is already live.
    discovery: tokio::sync::RwLock<Option<(Arc<Endpoints>, Instant)>>,
    flows: DashMap<String, Pending>,
}

impl Oidc {
    pub fn new(config: OidcConfig) -> Oidc {
        Oidc {
            config,
            // A short timeout: a provider that has not answered in ten seconds
            // is not going to, and somebody is watching a blank tab.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent(concat!("gal/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            discovery: tokio::sync::RwLock::new(None),
            flows: DashMap::new(),
        }
    }

    /// The provider's endpoints, from cache while it is fresh.
    async fn endpoints(&self) -> anyhow::Result<Arc<Endpoints>> {
        if let Some((cached, at)) = self.discovery.read().await.as_ref() {
            if at.elapsed() < DISCOVERY_TTL {
                return Ok(cached.clone());
            }
        }

        let url = format!("{}/.well-known/openid-configuration", self.config.issuer);
        let response = self.http.get(&url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("discovery at {url} answered {}", response.status());
        }
        let found: Endpoints = response.json().await?;

        // The issuer identifies the provider and is half the primary key of
        // every identity row. A document served from the configured issuer that
        // *names* a different one means the two would file the same subject
        // under different keys, so refuse rather than pick one.
        if found.issuer.trim_end_matches('/') != self.config.issuer {
            anyhow::bail!(
                "discovery at {url} claims issuer {:?}, expected {:?}",
                found.issuer,
                self.config.issuer
            );
        }

        let found = Arc::new(found);
        *self.discovery.write().await = Some((found.clone(), Instant::now()));
        Ok(found)
    }

    /// Record a started sign-in, returning the secret to put in the cookie.
    fn begin(&self, state: String, verifier: String) -> String {
        self.flows.retain(|_, f| f.started.elapsed() < FLOW_TTL);
        if self.flows.len() >= MAX_FLOWS {
            // Full of flows that have not yet expired: drop the oldest rather
            // than refuse somebody who is legitimately signing in.
            let oldest = self
                .flows
                .iter()
                .min_by_key(|f| f.started)
                .map(|f| f.key().clone());
            if let Some(key) = oldest {
                self.flows.remove(&key);
            }
        }

        let secret = auth::generate_token();
        self.flows.insert(
            auth::hash_token(&secret),
            Pending {
                state,
                verifier,
                started: Instant::now(),
            },
        );
        secret
    }

    /// Consume a started sign-in. Removing it is what makes a callback URL
    /// single-use, so a replayed one finds nothing.
    fn take(&self, secret: &str) -> Option<Pending> {
        let (_, flow) = self.flows.remove(&auth::hash_token(secret))?;
        (flow.started.elapsed() < FLOW_TTL).then_some(flow)
    }
}

/// Send the browser to the provider.
pub async fn start(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(oidc) = state.oidc.as_ref() else {
        return crate::http::not_found("Single sign-on is not configured.");
    };
    // Rate-limited with the other unauthenticated endpoints: each start costs a
    // discovery lookup and a map entry on behalf of somebody who has not
    // identified themselves.
    if let Some(refusal) = state.check_auth_rate(&headers, None) {
        return refusal;
    }

    let endpoints = match oidc.endpoints().await {
        Ok(endpoints) => endpoints,
        Err(e) => {
            return crate::http::server_error(e.context("could not reach the identity provider"))
        }
    };

    // `state` guards the round trip; the verifier guards the code. Both are
    // fresh 256-bit secrets from the generator sessions already use.
    let flow_state = auth::generate_token();
    let verifier = auth::generate_token();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let secret = oidc.begin(flow_state.clone(), verifier);

    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
         &code_challenge={}&code_challenge_method=S256",
        endpoints.authorization_endpoint,
        urlencode(&oidc.config.client_id),
        urlencode(&oidc.config.redirect_url),
        urlencode(&oidc.config.scopes),
        urlencode(&flow_state),
        urlencode(&challenge),
    );

    redirect_with_cookies(&url, &[&flow_cookie(&secret, state.config.secure_cookies)])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// The provider refused — the person pressed cancel, most likely. Carried
    /// so the browser can be sent somewhere that says so.
    error: Option<String>,
    error_description: Option<String>,
    /// Parameters conforming providers append that this endpoint does not act
    /// on. They have to be named or `deny_unknown_fields` refuses a perfectly
    /// correct callback: `iss` is RFC 9207, `session_state` is OIDC session
    /// management, and providers commonly echo `scope`.
    #[allow(dead_code)]
    iss: Option<String>,
    #[allow(dead_code)]
    session_state: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

/// Receive the browser back from the provider and sign the person in.
pub async fn callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = state.oidc.as_ref() else {
        return crate::http::not_found("Single sign-on is not configured.");
    };
    if let Some(refusal) = state.check_auth_rate(&headers, None) {
        return refusal;
    }

    // However this ends, the flow cookie has done its job and must not survive
    // to be replayed against a later sign-in.
    let clear = clear_flow_cookie(state.config.secure_cookies);

    if let Some(error) = query.error {
        tracing::info!(
            error,
            detail = query.error_description.unwrap_or_default(),
            "the identity provider refused a sign-in"
        );
        return redirect_with_cookies("/", &[&clear]);
    }

    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, FLOW_COOKIE));
    let Some(cookie) = cookie else {
        return crate::http::bad_request("That sign-in did not start here.");
    };
    let Some(flow) = oidc.take(&cookie) else {
        return crate::http::bad_request("That sign-in has expired. Please try again.");
    };

    // The returned `state` must match what this browser was given. Without the
    // check, somebody who completes a flow at the provider can feed their own
    // authorization code to another person's browser and silently sign that
    // person into the attacker's account.
    let returned = query.state.unwrap_or_default();
    if !auth::constant_time_eq(returned.as_bytes(), flow.state.as_bytes()) {
        return crate::http::bad_request("That sign-in did not start here.");
    }
    let Some(code) = query.code else {
        return crate::http::bad_request("The identity provider returned no code.");
    };

    let endpoints = match oidc.endpoints().await {
        Ok(endpoints) => endpoints,
        Err(e) => {
            return crate::http::server_error(e.context("could not reach the identity provider"))
        }
    };
    let claims = match exchange(oidc, &endpoints, &code, &flow.verifier).await {
        Ok(claims) => claims,
        Err(e) => return crate::http::server_error(e),
    };
    if claims.sub.trim().is_empty() {
        return crate::http::server_error(anyhow::anyhow!(
            "the identity provider's userinfo response carries no subject"
        ));
    }

    // The provider's name for someone is a starting point, not an account name.
    // Storage numbers it if it is taken.
    let hint = claims
        .preferred_username
        .as_deref()
        .and_then(auth::name_from_external)
        .or_else(|| claims.name.as_deref().and_then(auth::name_from_external))
        .unwrap_or_else(|| "user".to_string());
    let display = claims
        .name
        .filter(|n| !n.trim().is_empty() && n.chars().count() <= 64)
        .unwrap_or_else(|| hint.clone());

    let outcome = state
        .db
        .user_for_oauth_identity(endpoints.issuer.clone(), claims.sub, hint, display)
        .await;
    let user = match outcome {
        Ok(OauthLogin::Existing(user)) => user,
        Ok(OauthLogin::Created(user)) => {
            // Same as registration: make the account visible to waves that are
            // already resident, or it renders as an unknown id until a restart.
            state.cache_user(user.public()).await;
            tracing::info!(name = %user.name, "created an account from a provider sign-in");
            user
        }
        Err(e) => return crate::http::server_error(e),
    };

    let token = auth::generate_token();
    if let Err(e) = state
        .db
        .create_session(&user.id, auth::hash_token(&token))
        .await
    {
        return crate::http::server_error(e);
    }

    // The session rides home in the cookie alone, which is all the client has
    // ever used — `boot()` asks `/api/me` and takes the answer. Putting a token
    // in the URL would write a live credential into the browser's history for
    // no benefit.
    let session = auth::session_cookie(&token, state.config.secure_cookies, SESSION_TTL_MS / 1000);
    redirect_with_cookies("/", &[&session, &clear])
}

/// Trade the authorization code for an access token, then read the claims.
async fn exchange(
    oidc: &Oidc,
    endpoints: &Endpoints,
    code: &str,
    verifier: &str,
) -> anyhow::Result<UserInfo> {
    // Encoded by hand rather than with reqwest's `form` feature, which would
    // add `serde_urlencoded` to the tree to join five known pairs with `&`.
    let body = form_urlencode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", oidc.config.redirect_url.as_str()),
        // Sent alongside the Basic header because some providers identify the
        // client from the body whatever it authenticated with.
        ("client_id", oidc.config.client_id.as_str()),
        ("code_verifier", verifier),
    ]);
    let response = oidc
        .http
        .post(&endpoints.token_endpoint)
        // `client_secret_basic`: RFC 6749 §2.3.1 requires servers to support
        // it, where accepting the secret as a form field is only optional.
        .basic_auth(&oidc.config.client_id, Some(&oidc.config.client_secret))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        // The body names the reason — `invalid_grant`, a redirect URI that does
        // not match what was registered — and is the difference between a
        // five-minute fix and an afternoon. `server_error` logs it and sends
        // the caller a flat "Something went wrong."
        let detail = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "the token endpoint answered {status}: {}",
            detail.chars().take(500).collect::<String>()
        );
    }
    let token: TokenResponse = response.json().await?;

    let claims = oidc
        .http
        .get(&endpoints.userinfo_endpoint)
        .bearer_auth(&token.access_token)
        .send()
        .await?;
    let status = claims.status();
    if !status.is_success() {
        anyhow::bail!("the userinfo endpoint answered {status}");
    }
    Ok(claims.json().await?)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// The claims this application reads. Everything else a provider sends is
/// ignored rather than stored.
#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
}

/// A redirect that also sets cookies.
fn redirect_with_cookies(target: &str, cookies: &[&str]) -> Response {
    let mut response = Redirect::to(target).into_response();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// `SameSite=Lax` because the provider returns the browser by a top-level GET,
/// which `Strict` would strip the cookie from — every sign-in would then fail
/// on the last hop.
fn flow_cookie(secret: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{FLOW_COOKIE}={secret}; Path=/api/oauth; HttpOnly; SameSite=Lax; Max-Age={}",
        FLOW_TTL.as_secs()
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn clear_flow_cookie(secure: bool) -> String {
    let mut cookie = format!("{FLOW_COOKIE}=; Path=/api/oauth; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Pull one cookie out of a `Cookie` header by name.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// Percent-encode a value for a query string.
///
/// Written out rather than pulled in: the set of characters that may appear
/// unescaped is small and fixed, and this is the only place the server builds a
/// URL for somebody else to parse.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Join key/value pairs into an `application/x-www-form-urlencoded` body.
fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Report the configured provider to the login screen, so it knows whether to
/// draw a button. `None` when there is none.
pub fn provider_info(state: &AppState) -> Option<serde_json::Value> {
    state.oidc.as_ref().map(|oidc| {
        serde_json::json!({
            "label": oidc.config.label,
            "start": "/api/oauth/start",
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Oidc {
        Oidc::new(OidcConfig {
            issuer: "https://idp.example.com".into(),
            client_id: "id".into(),
            client_secret: "secret".into(),
            redirect_url: "https://gal.example.com/api/oauth/callback".into(),
            scopes: "openid profile".into(),
            label: "idp.example.com".into(),
        })
    }

    #[test]
    fn a_flow_can_be_spent_only_once() {
        let oidc = provider();
        let secret = oidc.begin("state".into(), "verifier".into());
        assert!(oidc.take(&secret).is_some());
        // A replayed callback URL finds nothing, so a code cannot be presented
        // twice.
        assert!(oidc.take(&secret).is_none());
    }

    #[test]
    fn only_the_digest_of_the_flow_secret_is_held() {
        let oidc = provider();
        let secret = oidc.begin("state".into(), "verifier".into());
        assert!(
            !oidc.flows.iter().any(|f| f.key() == &secret),
            "the cookie's value must not be usable as a key on its own"
        );
        assert!(oidc.flows.contains_key(&auth::hash_token(&secret)));
    }

    #[test]
    fn flows_in_flight_are_capped() {
        let oidc = provider();
        for _ in 0..MAX_FLOWS + 50 {
            oidc.begin("state".into(), "verifier".into());
        }
        assert!(
            oidc.flows.len() <= MAX_FLOWS,
            "an unauthenticated endpoint must not grow the map without bound, got {}",
            oidc.flows.len()
        );
    }

    #[test]
    fn the_pkce_challenge_is_the_sha256_of_the_verifier() {
        // The pair RFC 7636 appendix B fixes, so a mistake in the encoding
        // shows up here rather than as a provider refusing every sign-in.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn urlencoding_escapes_what_would_otherwise_break_the_query() {
        assert_eq!(urlencode("openid profile"), "openid%20profile");
        assert_eq!(
            urlencode("https://a.example.com/cb?x=1&y=2"),
            "https%3A%2F%2Fa.example.com%2Fcb%3Fx%3D1%26y%3D2"
        );
        // Unreserved characters survive untouched.
        assert_eq!(urlencode("aZ0-_.~"), "aZ0-_.~");
    }

    #[test]
    fn the_flow_cookie_is_scoped_and_out_of_reach_of_script() {
        let cookie = flow_cookie("s", true);
        assert!(cookie.contains("HttpOnly"));
        assert!(
            cookie.contains("SameSite=Lax"),
            "Strict would strip it from the provider's redirect back"
        );
        // Scoped to the only path that reads it, so it is not attached to every
        // request the client makes.
        assert!(cookie.contains("Path=/api/oauth"));
        assert!(cookie.contains("; Secure"));
        assert!(!flow_cookie("s", false).contains("; Secure"));
        assert!(clear_flow_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn the_flow_cookie_is_found_among_others() {
        let header = "theme=dark; gal_oauth=abc123; gal_session=xyz";
        assert_eq!(
            cookie_value(header, FLOW_COOKIE),
            Some("abc123".to_string())
        );
        assert_eq!(cookie_value("theme=dark", FLOW_COOKIE), None);
    }
}
