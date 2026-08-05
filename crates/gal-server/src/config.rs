//! Server configuration, read from the environment with usable defaults.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: SocketAddr,
    pub database: PathBuf,
    /// Adds `Secure` to the session cookie, so browsers only send it over
    /// HTTPS. Set this whenever Gal is behind a TLS-terminating proxy; leave it
    /// off for plain-HTTP local development, where a `Secure` cookie would be
    /// discarded and login would silently appear to do nothing.
    pub secure_cookies: bool,
    /// Allow new accounts to be created through the sign-up form.
    pub open_registration: bool,
    /// Origins permitted to open a WebSocket, beyond the server's own.
    ///
    /// Empty means same-origin only, which is what a normal deployment wants.
    /// Set it when the client is served from a different host than the API.
    pub allowed_origins: Vec<String>,
    /// Trust `X-Forwarded-For` for the client address.
    ///
    /// Only enable this behind a proxy that overwrites the header. If Gal is
    /// directly reachable, a client can set it freely and defeat rate limiting.
    pub trust_forwarded_for: bool,
    /// Send HSTS. Only meaningful behind HTTPS, and harmful before it, since a
    /// browser that caches it cannot reach a plain-HTTP deployment afterwards.
    pub hsts: bool,
    /// Bearer token a `/metrics` scrape must present.
    ///
    /// `None` disables the endpoint entirely, which is the default. Metrics
    /// describe how busy a server is, how many people are on it and when — so
    /// the endpoint is off until an operator says otherwise, rather than open
    /// and relying on a proxy rule nobody wrote.
    pub metrics_token: Option<String>,
    /// Emit logs as JSON rather than for a human reader.
    pub log_json: bool,
    /// An external OpenID Connect provider people may sign in through.
    ///
    /// `None` — the default — leaves the server exactly as it was, with
    /// passwords the only way in.
    pub oidc: Option<OidcConfig>,
}

/// Settings for signing in through an external OpenID Connect provider.
///
/// Either all of it is configured or none of it is. A half-configured provider
/// is a button that only fails once the person has already been bounced to
/// somebody else's login page, so the missing pieces are a startup error
/// instead.
#[derive(Clone)]
pub struct OidcConfig {
    /// Issuer URL, without a trailing slash. The endpoints are discovered from
    /// `{issuer}/.well-known/openid-configuration` rather than configured one
    /// by one — that document is the part of OIDC every provider implements,
    /// and depending on it is what keeps this vendor-neutral.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Where the provider sends the browser back to. Configured rather than
    /// derived from the request, because a redirect URI assembled from the
    /// `Host` header is one proxy misconfiguration away from sending
    /// authorization codes elsewhere — and because it has to match what the
    /// provider has registered exactly, so it should be the same string in
    /// both places.
    pub redirect_url: String,
    /// Space-separated scopes. `openid` is what makes it an OIDC request at
    /// all; `profile` is what carries a username worth naming an account after.
    pub scopes: String,
    /// What the sign-in button calls this provider.
    pub label: String,
}

/// Redact the secret. `Config` derives `Debug` and is logged at startup; the
/// derived version would put a client secret in the log aggregator.
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_url", &self.redirect_url)
            .field("scopes", &self.scopes)
            .field("label", &self.label)
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            database: PathBuf::from("gal.db"),
            secure_cookies: false,
            open_registration: true,
            allowed_origins: Vec::new(),
            trust_forwarded_for: false,
            hsts: false,
            metrics_token: None,
            log_json: false,
            oidc: None,
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let mut config = Config::default();

        if let Ok(host) = std::env::var("GAL_HOST") {
            let ip: IpAddr = host
                .parse()
                .map_err(|_| anyhow::anyhow!("GAL_HOST must be an IP address, got {host:?}"))?;
            config.addr.set_ip(ip);
        }
        if let Ok(port) = std::env::var("GAL_PORT") {
            let port: u16 = port
                .parse()
                .map_err(|_| anyhow::anyhow!("GAL_PORT must be a port number, got {port:?}"))?;
            config.addr.set_port(port);
        }
        if let Ok(path) = std::env::var("GAL_DB") {
            config.database = PathBuf::from(path);
        }
        if let Ok(value) = std::env::var("GAL_SECURE_COOKIES") {
            config.secure_cookies = parse_bool("GAL_SECURE_COOKIES", &value)?;
        }
        if let Ok(value) = std::env::var("GAL_OPEN_REGISTRATION") {
            config.open_registration = parse_bool("GAL_OPEN_REGISTRATION", &value)?;
        }
        if let Ok(value) = std::env::var("GAL_ALLOWED_ORIGINS") {
            config.allowed_origins = value
                .split(',')
                .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
                .filter(|o| !o.is_empty())
                .collect();
        }
        if let Ok(value) = std::env::var("GAL_TRUST_FORWARDED_FOR") {
            config.trust_forwarded_for = parse_bool("GAL_TRUST_FORWARDED_FOR", &value)?;
        }
        if let Ok(value) = std::env::var("GAL_HSTS") {
            config.hsts = parse_bool("GAL_HSTS", &value)?;
        }
        if let Ok(value) = std::env::var("GAL_METRICS_TOKEN") {
            let value = value.trim().to_string();
            // A short token on an endpoint that describes your traffic is worse
            // than no endpoint, and an empty one would open it to anyone who
            // sends an empty bearer. Refuse rather than approximate.
            if !value.is_empty() {
                if value.len() < 16 {
                    anyhow::bail!("GAL_METRICS_TOKEN must be at least 16 characters");
                }
                config.metrics_token = Some(value);
            }
        }
        if let Ok(value) = std::env::var("GAL_LOG_JSON") {
            config.log_json = parse_bool("GAL_LOG_JSON", &value)?;
        }
        config.oidc = OidcConfig::from_env()?;
        Ok(config)
    }
}

impl OidcConfig {
    /// Read the provider settings, or `None` when `GAL_OIDC_ISSUER` is unset.
    fn from_env() -> anyhow::Result<Option<OidcConfig>> {
        let Some(issuer) = non_empty("GAL_OIDC_ISSUER") else {
            return Ok(None);
        };
        // A trailing slash here becomes a double slash in the discovery URL,
        // which some providers answer with a 404.
        let issuer = issuer.trim_end_matches('/').to_string();
        require_web_url("GAL_OIDC_ISSUER", &issuer)?;

        let client_id = non_empty("GAL_OIDC_CLIENT_ID")
            .ok_or_else(|| anyhow::anyhow!("GAL_OIDC_CLIENT_ID is required when OIDC is on"))?;
        let client_secret = non_empty("GAL_OIDC_CLIENT_SECRET")
            .ok_or_else(|| anyhow::anyhow!("GAL_OIDC_CLIENT_SECRET is required when OIDC is on"))?;
        let redirect_url = non_empty("GAL_OIDC_REDIRECT_URL")
            .ok_or_else(|| anyhow::anyhow!("GAL_OIDC_REDIRECT_URL is required when OIDC is on"))?;
        require_web_url("GAL_OIDC_REDIRECT_URL", &redirect_url)?;

        let scopes = non_empty("GAL_OIDC_SCOPES").unwrap_or_else(|| "openid profile".to_string());
        if !scopes.split_whitespace().any(|s| s == "openid") {
            anyhow::bail!("GAL_OIDC_SCOPES must include `openid`");
        }
        // Naming the host beats naming nobody: "Sign in with git.example.com"
        // tells someone where they are about to be sent.
        let label = non_empty("GAL_OIDC_LABEL")
            .unwrap_or_else(|| host_of(&issuer).unwrap_or("your provider").to_string());

        Ok(Some(OidcConfig {
            issuer,
            client_id,
            client_secret,
            redirect_url,
            scopes,
            label,
        }))
    }
}

/// A set, non-blank environment variable, trimmed.
///
/// Unset and empty mean the same thing, matching [`parse_bool`]: `GAL_OIDC_ISSUER=`
/// is how an operator switches the feature off without deleting the line.
fn non_empty(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Require an absolute `http`/`https` URL, and require `https` unless it points
/// at this machine.
///
/// The whole flow rests on TLS: the authorization code, the client secret and
/// the access token all cross that connection, and the ID token's signature is
/// deliberately not checked because TLS is doing that job (OIDC Core §3.1.3.7).
/// Plain `http` to a loopback address is the exception every OAuth
/// implementation makes — that traffic never leaves the host, and it is how
/// anyone develops against a provider running next to them.
fn require_web_url(name: &str, url: &str) -> anyhow::Result<()> {
    let Some((scheme, host)) = scheme_and_host(url) else {
        anyhow::bail!("{name} must be an absolute http:// or https:// URL, got {url:?}");
    };
    if scheme == "http" && !is_loopback_host(host) {
        anyhow::bail!(
            "{name} refuses plain http to {host:?} — use https, or a loopback address for local development"
        );
    }
    Ok(())
}

/// Split an absolute web URL into its scheme and bare host.
fn scheme_and_host(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo first, or `user@host` reads as a host of `user`.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal's colons are not a port separator.
        Some(rest) => rest.split_once(']')?.0,
        None => authority.split(':').next()?,
    };
    (!host.is_empty()).then_some((scheme, host))
}

fn host_of(url: &str) -> Option<&str> {
    scheme_and_host(url).map(|(_, host)| host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip: IpAddr| ip.is_loopback())
}

/// Reads a boolean setting, refusing a spelling that is neither on nor off.
///
/// Treating an unrecognised value as false is how `GAL_SECURE_COOKIES=ture` and
/// `GAL_HSTS=enabled` started a server with the protection the operator was
/// asking for switched off, and said nothing. `GAL_HOST` and `GAL_PORT` already
/// refuse a value they cannot read; these matter more, not less.
///
/// An empty value stays false, which is documented and deliberate: it is how a
/// setting is turned off without unsetting the variable.
fn parse_bool(name: &str, value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(anyhow::anyhow!(
            "{name} must be one of 1/true/yes/on or 0/false/no/off, got {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_accepts_common_spellings() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(parse_bool("X", yes).unwrap(), "{yes:?} should be true");
        }
        // Empty stays false: that is how a setting is turned off without
        // unsetting the variable, and it is documented.
        for no in ["0", "false", "no", "off", ""] {
            assert!(!parse_bool("X", no).unwrap(), "{no:?} should be false");
        }
    }

    #[test]
    fn urls_split_into_scheme_and_host() {
        assert_eq!(
            scheme_and_host("https://git.example.com/path?q=1#f"),
            Some(("https", "git.example.com"))
        );
        assert_eq!(
            scheme_and_host("http://127.0.0.1:3000"),
            Some(("http", "127.0.0.1"))
        );
        // The colons inside an IPv6 literal are not a port, and userinfo is not
        // a host.
        assert_eq!(
            scheme_and_host("http://[::1]:3000/x"),
            Some(("http", "::1"))
        );
        assert_eq!(
            scheme_and_host("https://user@example.com/"),
            Some(("https", "example.com"))
        );

        for bad in [
            "example.com",
            "ftp://example.com",
            // A `javascript:` URL must not survive as a redirect target.
            "javascript://example.com",
            "https://",
        ] {
            assert_eq!(scheme_and_host(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn plain_http_is_permitted_only_to_this_machine() {
        // The flow's confidentiality rests on TLS, so http off-box is refused
        // rather than warned about.
        require_web_url("X", "https://git.example.com").unwrap();
        require_web_url("X", "http://localhost:3000").unwrap();
        require_web_url("X", "http://127.0.0.1:3000").unwrap();
        require_web_url("X", "http://[::1]:3000").unwrap();

        let err = require_web_url("X", "http://git.example.com").unwrap_err();
        assert!(err.to_string().contains("refuses plain http"), "{err}");
        assert!(require_web_url("X", "not a url").is_err());
    }

    #[test]
    fn something_that_merely_looks_like_loopback_is_not() {
        // `localhost.example.com` is somebody else's domain, and
        // `127.0.0.1.evil.net` is not an address at all.
        assert!(is_loopback_host("localhost") && is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("127.0.0.1") && is_loopback_host("127.9.9.9"));
        assert!(!is_loopback_host("localhost.example.com"));
        assert!(!is_loopback_host("127.0.0.1.evil.net"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn the_client_secret_stays_out_of_the_debug_output() {
        let config = Config {
            oidc: Some(OidcConfig {
                issuer: "https://git.example.com".into(),
                client_id: "id".into(),
                client_secret: "hunter2-the-real-secret".into(),
                redirect_url: "https://gal.example.com/api/oauth/callback".into(),
                scopes: "openid profile".into(),
                label: "git.example.com".into(),
            }),
            ..Config::default()
        };
        // The whole `Config` is what gets logged, so that is what is checked.
        let shown = format!("{config:?}");
        assert!(
            !shown.contains("hunter2"),
            "the secret reached a log line: {shown}"
        );
        assert!(shown.contains("<redacted>"));
    }

    /// Falling back to false meant a typo started the server with the
    /// protection the operator asked for switched off, silently.
    #[test]
    fn an_unrecognised_spelling_is_refused_rather_than_read_as_off() {
        for bad in ["ture", "enabled", "y", "TRUE!", "2"] {
            let err = parse_bool("GAL_SECURE_COOKIES", bad).unwrap_err();
            assert!(
                err.to_string().contains("GAL_SECURE_COOKIES"),
                "should name the variable: {err}"
            );
        }
    }
}
