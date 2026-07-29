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
            config.secure_cookies = is_truthy(&value);
        }
        if let Ok(value) = std::env::var("GAL_OPEN_REGISTRATION") {
            config.open_registration = is_truthy(&value);
        }
        if let Ok(value) = std::env::var("GAL_ALLOWED_ORIGINS") {
            config.allowed_origins = value
                .split(',')
                .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
                .filter(|o| !o.is_empty())
                .collect();
        }
        if let Ok(value) = std::env::var("GAL_TRUST_FORWARDED_FOR") {
            config.trust_forwarded_for = is_truthy(&value);
        }
        if let Ok(value) = std::env::var("GAL_HSTS") {
            config.hsts = is_truthy(&value);
        }
        Ok(config)
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_accepts_common_spellings() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(yes), "{yes:?} should be true");
        }
        for no in ["0", "false", "no", "off", ""] {
            assert!(!is_truthy(no), "{no:?} should be false");
        }
    }
}
