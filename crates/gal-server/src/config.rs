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
        Ok(config)
    }
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
