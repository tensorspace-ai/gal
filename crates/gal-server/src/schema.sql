-- Gal storage schema.
--
-- Durability model: every accepted op is appended to `ops` before it is
-- acknowledged, and `blips.content` holds a materialised snapshot so opening a
-- wave never has to replay history. The two are written in one transaction, so
-- the snapshot can never drift from the log.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,   -- lowercase login handle
    display_name  TEXT NOT NULL,
    email         TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    color         INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);

-- Only the hash of a session token is stored, so a database leak does not hand
-- out live sessions.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS sessions_expiry ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS waves (
    id         TEXT PRIMARY KEY,
    creator    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS wavelets (
    id            TEXT PRIMARY KEY,
    wave_id       TEXT NOT NULL REFERENCES waves(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('conversation', 'privateReply')),
    title         TEXT NOT NULL DEFAULT '',
    anchor_blip   TEXT,
    created_at    INTEGER NOT NULL,
    last_modified INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS wavelets_wave ON wavelets(wave_id);

-- Membership is the access rule: a user may see a wavelet if and only if they
-- have a row here.
CREATE TABLE IF NOT EXISTS participants (
    wavelet_id TEXT NOT NULL REFERENCES wavelets(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (wavelet_id, user_id)
);
CREATE INDEX IF NOT EXISTS participants_user ON participants(user_id);

CREATE TABLE IF NOT EXISTS blips (
    id            TEXT PRIMARY KEY,
    wavelet_id    TEXT NOT NULL REFERENCES wavelets(id) ON DELETE CASCADE,
    wave_id       TEXT NOT NULL,
    parent        TEXT,                  -- NULL for a root-level blip
    seq           INTEGER NOT NULL,      -- sibling ordering within a wavelet
    author        TEXT NOT NULL,
    contributors  TEXT NOT NULL,         -- JSON array of user ids
    created_at    INTEGER NOT NULL,
    last_modified INTEGER NOT NULL,
    content       TEXT NOT NULL,         -- JSON delta snapshot
    revision      INTEGER NOT NULL,
    deleted       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS blips_wavelet ON blips(wavelet_id, seq);
CREATE INDEX IF NOT EXISTS blips_wave ON blips(wave_id, last_modified);

-- The append-only op log. Powers playback and lets a document be rebuilt from
-- scratch if a snapshot is ever suspect.
CREATE TABLE IF NOT EXISTS ops (
    blip_id   TEXT NOT NULL,
    revision  INTEGER NOT NULL,
    wave_id   TEXT NOT NULL,
    author    TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    delta     TEXT NOT NULL,
    -- Client-generated, unique per submitted op. Lets a reconnecting client
    -- replay pending work without the risk of applying it twice; the durable
    -- record is what makes this survive the wave being evicted from memory.
    op_id     TEXT,
    PRIMARY KEY (blip_id, revision)
);
CREATE UNIQUE INDEX IF NOT EXISTS ops_op_id ON ops(blip_id, op_id) WHERE op_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS ops_wave_time ON ops(wave_id, timestamp);

-- Read state is per blip *revision*, so editing a blip makes it unread again.
CREATE TABLE IF NOT EXISTS read_marks (
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blip_id  TEXT NOT NULL,
    revision INTEGER NOT NULL,
    PRIMARY KEY (user_id, blip_id)
);

CREATE TABLE IF NOT EXISTS wave_flags (
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    wave_id  TEXT NOT NULL REFERENCES waves(id) ON DELETE CASCADE,
    archived INTEGER NOT NULL DEFAULT 0,
    muted    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, wave_id)
);

-- Full-text index over blip text. Kept in step with `blips` by the application
-- rather than by triggers, because the indexed form is the delta's plain-text
-- projection, which SQL cannot compute.
CREATE VIRTUAL TABLE IF NOT EXISTS blip_search USING fts5(
    blip_id UNINDEXED,
    wave_id UNINDEXED,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
