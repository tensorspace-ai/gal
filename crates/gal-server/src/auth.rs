//! Accounts, password hashing and session cookies.
//!
//! Passwords are hashed with Argon2id. Session tokens are 256 bits of OS
//! randomness; only their SHA-256 hash is stored, so a leaked database cannot be
//! replayed as a set of live logins.

use anyhow::{anyhow, Result};
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use base64::Engine;
use gal_core::model::User;
use rand::RngCore;

/// Name of the cookie carrying the session token.
pub const COOKIE_NAME: &str = "gal_session";

/// Hash a password for storage.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("failed to hash password: {e}"))
}

/// Check a password against a stored hash.
///
/// Returns `false` rather than an error for a bad password; a malformed stored
/// hash is also treated as a failed login rather than a server error, so a
/// corrupt row cannot be used to probe for account existence.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generate a fresh session token. The plaintext goes to the browser; only
/// [`hash_token`] of it is ever persisted.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Hash a session token for storage and lookup.
pub fn hash_token(token: &str) -> String {
    // SHA-256 via a tiny dependency-free implementation would be overkill here;
    // Argon2's crate family already brings in a SHA-2 implementation.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Compare two secrets without letting the time taken reveal how much of one
/// was guessed.
///
/// Both sides are hashed first. That makes the comparison fixed-length as well
/// as constant-time in the content: a plain byte loop returns early when the
/// lengths differ, which hands an attacker the length of the secret for free.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let (a, b) = (Sha256::digest(a), Sha256::digest(b));
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

/// Build the `Set-Cookie` header value for a new session.
pub fn session_cookie(token: &str, secure: bool, max_age_secs: i64) -> String {
    let mut cookie =
        format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Build the `Set-Cookie` header value that clears a session.
pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Pull the session token out of a `Cookie` header.
pub fn token_from_cookies(header: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| value.trim().to_string())
    })
}

/// Why a registration attempt was refused.
#[derive(Debug, PartialEq)]
pub enum SignupError {
    NameTooShort,
    NameTooLong,
    NameCharacters,
    PasswordTooShort,
    PasswordTooLong,
    PasswordTooCommon,
    PasswordContainsName,
    DisplayNameTooLong,
}

impl std::fmt::Display for SignupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            SignupError::NameTooShort => "Username must be at least 2 characters.",
            SignupError::NameTooLong => "Username must be at most 32 characters.",
            SignupError::NameCharacters => {
                "Username may only contain letters, numbers, dots, dashes and underscores."
            }
            SignupError::PasswordTooShort => "Password must be at least 12 characters.",
            SignupError::PasswordTooLong => "Password must be at most 1024 characters.",
            SignupError::PasswordTooCommon => {
                "That password is one of the most common ones in use. Please choose another."
            }
            SignupError::PasswordContainsName => "Password must not contain your username.",
            SignupError::DisplayNameTooLong => "Display name must be at most 64 characters.",
        };
        f.write_str(msg)
    }
}

/// Shortest password accepted.
///
/// Raised from eight. Eight characters of anything, with no other rule, is
/// inside the reach of an offline attack on a leaked hash and well inside the
/// reach of an online one spread across enough addresses to stay under a
/// per-address limiter. Existing passwords keep working; this applies when one
/// is chosen.
const MIN_PASSWORD: usize = 12;

/// Longest password accepted.
///
/// Not a strength rule. Argon2's cost grows with the length of its input, and
/// nothing bounded it, so a single registration carrying a ten-megabyte
/// password was a cheap way to spend a lot of somebody else's CPU — on the
/// blocking pool that logins share.
const MAX_PASSWORD: usize = 1024;

/// Passwords common enough that an online attack will try them within its first
/// few guesses, plus the ones this application's own words invite.
///
/// This is deliberately a short list and not a breach corpus: the honest fix is
/// a k-anonymity check against a service like Have I Been Pwned, which means
/// making a network request during registration and deciding what to do when it
/// fails. Until that exists this catches the handful that a rate limiter alone
/// would still eventually let through.
/// Every entry must be at least `MIN_PASSWORD` long or it is unreachable —
/// anything shorter is refused for its length before it gets here. There is a
/// test for that, because the first draft of this list was mostly dead.
const COMMON_PASSWORDS: &[&str] = &[
    "123456789012",
    "1234567890123",
    "12345678901234",
    "111111111111",
    "aaaaaaaaaaaa",
    "password1234",
    "passw0rd1234",
    "passwordpassword",
    "qwertyuiop123",
    "qwertyuiopasdfghjkl",
    "administrator",
    "iloveyou1234",
    "letmein123456",
    "welcome123456",
    "abcdefghijklmnop",
    "thisisapassword",
    // Famous enough to be in every wordlist that matters, xkcd notwithstanding.
    "correcthorsebatterystaple",
];

/// Check a password on its own terms, for registration and for a change alike.
///
/// `name` is the account's username, when there is one to compare against: a
/// password that contains the handle it protects is the first thing anyone
/// guesses, and it is the one weak password a length rule cannot catch.
pub fn validate_password(password: &str, name: Option<&str>) -> Result<(), SignupError> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(SignupError::PasswordTooShort);
    }
    if password.chars().count() > MAX_PASSWORD {
        return Err(SignupError::PasswordTooLong);
    }

    let folded = password.to_lowercase();
    if COMMON_PASSWORDS.contains(&folded.as_str()) {
        return Err(SignupError::PasswordTooCommon);
    }
    if let Some(name) = name {
        let name = name.trim().to_lowercase();
        if name.chars().count() >= 3 && folded.contains(&name) {
            return Err(SignupError::PasswordContainsName);
        }
    }
    Ok(())
}

/// Validate and normalise sign-up input.
///
/// Returns the canonical (lowercased, trimmed) username and display name.
/// Check an account name on its own terms, returning it canonicalised.
///
/// Split out from [`validate_signup`] so that a name arriving from somewhere
/// other than the sign-up form — an identity provider, say — is held to exactly
/// the same rule rather than to a second copy of it that can drift.
pub fn validate_name(name: &str) -> Result<String, SignupError> {
    let name = name.trim().to_lowercase();
    if name.chars().count() < 2 {
        return Err(SignupError::NameTooShort);
    }
    if name.chars().count() > 32 {
        return Err(SignupError::NameTooLong);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(SignupError::NameCharacters);
    }
    Ok(name)
}

pub fn validate_signup(
    name: &str,
    display_name: &str,
    password: &str,
) -> Result<(String, String), SignupError> {
    let name = validate_name(name)?;
    validate_password(password, Some(&name))?;

    // Fall back to the handle when no display name is given, so a user always
    // has something to render.
    let display = display_name.trim();
    let display = if display.is_empty() {
        name.clone()
    } else {
        display.to_string()
    };
    if display.chars().count() > 64 {
        return Err(SignupError::DisplayNameTooLong);
    }
    Ok((name, display))
}

/// Derive an account name from whatever an external identity provider calls
/// someone.
///
/// Providers are not bound by our rules. `preferred_username` is commonly an
/// email address, and may carry capitals, spaces, accents, or characters this
/// application does not allow in a handle — so this is a *derivation*, not a
/// validation: unusable characters are dropped, and the caller numbers the
/// result if it collides with an account that already exists.
///
/// An address is cut at the `@`. `alice@example.com` becoming `aliceexample.com`
/// helps nobody, and the local part is the name the person actually goes by.
///
/// Returns `None` when nothing usable survives — an all-emoji display name, or
/// one that reduces to a single character, which is below the minimum a name
/// has to meet. The caller supplies a generic fallback rather than this
/// inventing one.
pub fn name_from_external(raw: &str) -> Option<String> {
    let local = raw.split('@').next().unwrap_or(raw);
    let mut name: String = local
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();

    name.truncate(32);
    let name = name.trim_matches(['.', '-', '_']).to_string();

    // Hold the result to the same rule a person signing up meets, rather than
    // trusting the filter above to have covered it — in particular the
    // two-character floor, which a provider name of `x` would fail.
    validate_name(&name).ok()
}

/// Decide whether a browser-initiated request may open a WebSocket.
///
/// Without this the only thing preventing a hostile page from opening an
/// authenticated socket is the cookie's `SameSite=Lax` attribute — a browser
/// default, not a server control. `Lax` is also same-*site*, so any sibling
/// subdomain would still qualify. An explicit check is the standard defence.
///
/// A missing `Origin` is allowed: non-browser clients omit it, and they cannot
/// obtain the victim's cookie in the first place, which is the whole premise of
/// the attack.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, allowed: &[String]) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    if origin == "null" {
        // Sandboxed iframes and `file://` pages. Never a legitimate client.
        return false;
    }
    if allowed.contains(&origin) {
        return true;
    }
    // Default: same origin. Compare the authority, since the scheme is not
    // observable behind a TLS-terminating proxy.
    match (origin.split_once("://"), host) {
        (Some((_, authority)), Some(host)) => authority == host.trim().to_ascii_lowercase(),
        _ => false,
    }
}

/// Everything the request handlers need to know about the caller.
#[derive(Clone, Debug)]
pub struct Identity {
    pub user: User,
    pub token_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_verifies_only_against_itself() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
        assert!(!verify_password("", &hash));
    }

    #[test]
    fn hashes_are_salted_so_equal_passwords_differ() {
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(
            a, b,
            "identical passwords must not produce identical hashes"
        );
        assert!(verify_password("same", &a) && verify_password("same", &b));
    }

    #[test]
    fn malformed_stored_hash_fails_closed() {
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn tokens_are_unique_and_hashing_is_stable() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "expected 256 bits of entropy, got {a:?}");
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert_ne!(hash_token(&a), a, "the raw token must not be what we store");
    }

    #[test]
    fn cookie_parsing_finds_the_session_among_others() {
        let header = "theme=dark; gal_session=abc123; other=x";
        assert_eq!(token_from_cookies(header), Some("abc123".to_string()));
        assert_eq!(token_from_cookies("theme=dark"), None);
        assert_eq!(token_from_cookies(""), None);
    }

    #[test]
    fn cookie_is_httponly_and_respects_the_secure_switch() {
        let cookie = session_cookie("tok", false, 60);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("Secure"));
        assert!(session_cookie("tok", true, 60).contains("; Secure"));
    }

    #[test]
    fn clearing_cookie_expires_it_immediately() {
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn origin_check_allows_same_origin_and_configured_origins() {
        let none: Vec<String> = vec![];
        // Same origin, http and https, with and without a port.
        assert!(origin_allowed(
            Some("http://gal.example.com"),
            Some("gal.example.com"),
            &none
        ));
        assert!(origin_allowed(
            Some("https://gal.example.com"),
            Some("gal.example.com"),
            &none
        ));
        assert!(origin_allowed(
            Some("http://127.0.0.1:8080"),
            Some("127.0.0.1:8080"),
            &none
        ));
        // Explicitly configured.
        let allowed = vec!["https://app.example.com".to_string()];
        assert!(origin_allowed(
            Some("https://app.example.com"),
            Some("gal.example.com"),
            &allowed
        ));
        // Non-browser clients omit Origin and cannot be CSWSH victims.
        assert!(origin_allowed(None, Some("gal.example.com"), &none));
    }

    #[test]
    fn origin_check_rejects_foreign_and_sibling_origins() {
        let none: Vec<String> = vec![];
        assert!(!origin_allowed(
            Some("https://evil.example.com"),
            Some("gal.example.com"),
            &none
        ));
        // A sibling subdomain is same-*site*, so SameSite=Lax would not stop it.
        assert!(!origin_allowed(
            Some("https://blog.example.com"),
            Some("gal.example.com"),
            &none
        ));
        // Sandboxed iframe / file:// origin.
        assert!(!origin_allowed(
            Some("null"),
            Some("gal.example.com"),
            &none
        ));
        // Prefix and suffix confusion.
        assert!(!origin_allowed(
            Some("https://gal.example.com.evil.net"),
            Some("gal.example.com"),
            &none
        ));
        assert!(!origin_allowed(
            Some("https://notgal.example.com"),
            Some("gal.example.com"),
            &none
        ));
    }

    #[test]
    fn signup_validation_normalises_and_rejects() {
        // Was "longenough", ten characters, which the minimum has outgrown.
        const GOOD: &str = "a reasonable passphrase";

        let (name, display) = validate_signup("  Alice  ", " Alice A ", GOOD).unwrap();
        assert_eq!(name, "alice", "handles are canonicalised to lowercase");
        assert_eq!(display, "Alice A");

        // Missing display name falls back to the handle.
        let (_, display) = validate_signup("bob", "  ", GOOD).unwrap();
        assert_eq!(display, "bob");

        assert_eq!(
            validate_signup("a", "A", GOOD),
            Err(SignupError::NameTooShort)
        );
        assert_eq!(
            validate_signup("bad name", "X", GOOD),
            Err(SignupError::NameCharacters)
        );
        assert_eq!(
            validate_signup("alice", "A", "short"),
            Err(SignupError::PasswordTooShort)
        );
        assert_eq!(
            validate_signup("a@b", "X", GOOD),
            Err(SignupError::NameCharacters)
        );
        assert_eq!(
            validate_signup("alice", &"x".repeat(65), GOOD),
            Err(SignupError::DisplayNameTooLong),
            "a display name that is too long said the opposite for a long time"
        );
    }

    #[test]
    fn a_provider_name_is_reduced_to_something_this_application_accepts() {
        assert_eq!(name_from_external("Alice"), Some("alice".into()));
        // An address is its local part, not the whole string with the @ removed.
        assert_eq!(
            name_from_external("alice.smith@example.com"),
            Some("alice.smith".into())
        );
        assert_eq!(name_from_external(" Bob Jones "), Some("bobjones".into()));
        assert_eq!(name_from_external("a.b-c_1"), Some("a.b-c_1".into()));

        // Nothing usable survives, so the caller has to supply a fallback
        // rather than being handed a name the rules would reject.
        assert_eq!(name_from_external("🎉🎉"), None);
        assert_eq!(name_from_external(""), None);
        assert_eq!(name_from_external("..."), None);
        // One character is below the floor, even though every character is
        // legal.
        assert_eq!(name_from_external("x"), None);
    }

    #[test]
    fn a_derived_name_always_passes_the_rule_it_was_derived_against() {
        // The filter and the validator must not disagree; whatever this returns
        // is about to become an account.
        for raw in [
            "Alice",
            "alice@example.com",
            "ALICE SMITH",
            "-alice-",
            "...alice...",
            "ab",
            "99",
            "José Müller",
            &"x".repeat(200),
        ] {
            if let Some(name) = name_from_external(raw) {
                validate_name(&name)
                    .unwrap_or_else(|e| panic!("{raw:?} derived {name:?}, which is invalid: {e}"));
            }
        }
    }

    #[test]
    fn a_password_is_judged_on_more_than_its_length() {
        assert!(validate_password("a reasonable passphrase", Some("alice")).is_ok());

        assert_eq!(
            validate_password("elevenchars", None),
            Err(SignupError::PasswordTooShort),
            "eleven is one short of the minimum"
        );

        // Not a strength rule: Argon2's cost grows with its input, and nothing
        // bounded it, so one registration could spend a lot of somebody else's
        // CPU on the pool that logins share.
        assert_eq!(
            validate_password(&"x".repeat(1025), None),
            Err(SignupError::PasswordTooLong)
        );

        assert_eq!(
            validate_password("password1234", None),
            Err(SignupError::PasswordTooCommon)
        );
        assert_eq!(
            validate_password("PassWord1234", None),
            Err(SignupError::PasswordTooCommon),
            "the check is case-insensitive, or it is trivially sidestepped"
        );

        // The one weak password a length rule cannot see.
        assert_eq!(
            validate_password("alice-in-the-wonderland", Some("alice")),
            Err(SignupError::PasswordContainsName)
        );
        assert_eq!(
            validate_password("ALICE in the wonderland", Some("Alice")),
            Err(SignupError::PasswordContainsName)
        );
        // A two-character handle appears inside too many ordinary words to
        // reject on, so it is not compared.
        assert!(validate_password("a reasonable passphrase", Some("ab")).is_ok());
    }

    /// The length check runs first, so a common password shorter than the
    /// minimum can never reach the list — it is refused for being short and the
    /// entry is decoration. Most of the first version of this list was exactly
    /// that: "password123" is eleven characters and unreachable behind a
    /// twelve-character minimum.
    #[test]
    fn every_common_password_is_long_enough_to_be_reachable() {
        for entry in COMMON_PASSWORDS {
            assert!(
                entry.chars().count() >= MIN_PASSWORD,
                "{entry:?} is shorter than the minimum, so it is dead weight"
            );
            assert_eq!(
                validate_password(entry, None),
                Err(SignupError::PasswordTooCommon),
                "{entry:?} should be refused as common, not for some other reason"
            );
        }
    }
}
