# Security Policy

## Reporting a vulnerability

Please report security issues privately. Do not open a public issue.

- **Email:** security@tensorspace.ai
- **GitHub:** use [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
  on this repository.

Please include the version or commit, a description of the issue, and the steps
to reproduce it. If you have a proof of concept, include it — it makes triage
much faster.

We aim to acknowledge a report within 3 working days and to ship a fix or a
mitigation plan within 30 days. We will credit you in the release notes unless
you ask us not to.

## Supported versions

Only the latest release receives security fixes.

## Threat model

Gal is a self-hosted server. Understanding what it does and does not defend
against will tell you whether a behaviour is a bug or a known limitation.

**Gal assumes:**

- It runs behind a TLS-terminating reverse proxy. Gal itself speaks plain HTTP.
- Registered users are semi-trusted. Any participant of a wave can read, edit,
  and delete content in that wave — that is the product, not a flaw.
- The operator controls who can register (`GAL_OPEN_REGISTRATION`).

**Gal defends against:**

- Reading or modifying a wave you do not participate in, including via search,
  playback, inbox summaries, presence, and live updates.
- Cross-site request forgery and cross-origin WebSocket connections.
- Credential theft from a database leak (Argon2id password hashes; only SHA-256
  hashes of session tokens are stored).
- Script injection through message content, display names, titles, links, and
  search snippets.

**Gal does not currently defend against:**

- A malicious *participant* of a wave abusing their access to that wave.
- Traffic-analysis or timing attacks by a network observer.
- Resource abuse beyond the built-in rate limits and quotas — a determined
  authenticated user can still generate significant load.

See the "Not implemented" section of the README for feature-level gaps.

## Known limitations

These are documented rather than fixed, and are not eligible for a security
advisory:

- **No end-to-end encryption.** The server can read all content. Private
  replies are enforced by the server, not by cryptography.
- **No account recovery.** There is no password reset, so losing a password
  means losing the account.
- **No audit log.** Participant changes and deletions are not recorded.
