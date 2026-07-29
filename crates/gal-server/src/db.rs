//! SQLite persistence.
//!
//! Every method is `async` and runs the actual query on the blocking pool, so a
//! slow disk can never stall the reactor. WAL mode lets readers proceed while a
//! writer holds the lock, and a busy timeout absorbs the brief contention when
//! two waves commit at once.

use std::path::Path;

use anyhow::{Context, Result};
use gal_core::model::*;
use gal_core::protocol::{PlaybackFrame, SearchHit, WaveSummary};
use gal_ot::Delta;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

pub type SqlitePool = Pool<SqliteConnectionManager>;

#[derive(Clone)]
pub struct Storage {
    pool: SqlitePool,
}

/// How long a login stays valid.
pub const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The schema version this build expects.
///
/// Bump it whenever `schema.sql` changes, and add the corresponding step to
/// [`migrate`]. `CREATE TABLE IF NOT EXISTS` is a no-op against an existing
/// table, so without this a new column would simply never be added: the server
/// would start cleanly, then fail at query time in ways that look like data loss
/// to the user.
pub const SCHEMA_VERSION: i64 = 2;

/// The version `schema.sql` describes.
///
/// `schema.sql` is a frozen baseline, not the current schema. Every change after
/// it is a migration step, so a fresh database and an upgraded one end up
/// identical, and a statement can never reference a column that an older
/// database has not been given yet.
const BASELINE_VERSION: i64 = 1;

impl Storage {
    /// Open (creating if needed) the database at `path`, and bring its schema up
    /// to [`SCHEMA_VERSION`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path.as_ref()).with_init(|conn| {
            // Applied per connection: pragmas are connection-scoped.
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA temp_store = MEMORY;",
            )
        });
        let pool = Pool::builder()
            .max_size(16)
            // r2d2 defaults to 30s, which turns "the path is unwritable" into
            // half a minute of silence followed by a misleading timeout error.
            .connection_timeout(std::time::Duration::from_secs(3))
            .build(manager)
            .context("failed to create the SQLite connection pool")?;

        let mut conn = pool.get().context("could not open the database")?;
        migrate(&mut conn)?;
        Ok(Storage { pool })
    }

    /// Run a blocking database closure on the thread pool.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().context("no database connection available")?;
            f(&mut conn)
        })
        .await
        .context("database task panicked")?
    }

    // --- users ----------------------------------------------------------

    pub async fn create_user(
        &self,
        name: String,
        display_name: String,
        email: String,
        password_hash: String,
    ) -> Result<User> {
        let user = User {
            id: UserId::new(),
            color: color_for(&name),
            name,
            display_name,
            email,
            password_hash,
            created_at: now(),
        };
        let stored = user.clone();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO users (id, name, display_name, email, password_hash, color, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    stored.id.as_str(),
                    stored.name,
                    stored.display_name,
                    stored.email,
                    stored.password_hash,
                    stored.color,
                    stored.created_at
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(user)
    }

    pub async fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        let name = name.to_lowercase();
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT * FROM users WHERE name = ?1",
                    params![name],
                    row_to_user,
                )
                .optional()?)
        })
        .await
    }

    /// People the user already shares a wave with, for rendering their name and
    /// avatar. Deliberately not the whole directory — see `find_user`.
    pub async fn known_users(&self, user_id: &UserId) -> Result<Vec<PublicUser>> {
        let id = user_id.clone();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT u.* FROM users u
                 JOIN participants p ON p.user_id = u.id
                 WHERE p.wavelet_id IN (
                     SELECT wavelet_id FROM participants WHERE user_id = ?1)
                 ORDER BY u.display_name",
            )?;
            let rows =
                stmt.query_map(params![id.as_str()], |r| row_to_user(r).map(|u| u.public()))?;
            let users = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(users)
        })
        .await
    }

    /// Every user. Internal use only — never expose this to a client, because a
    /// full member directory is exactly what an abuser needs to spam or target
    /// everyone on the server.
    pub async fn all_users(&self) -> Result<Vec<PublicUser>> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM users ORDER BY display_name")?;
            let rows = stmt.query_map([], |r| row_to_user(r).map(|u| u.public()))?;
            let users = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(users)
        })
        .await
    }

    pub async fn user_count(&self) -> Result<i64> {
        self.run(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?))
            .await
    }

    // --- sessions -------------------------------------------------------

    pub async fn create_session(&self, user_id: &UserId, token_hash: String) -> Result<()> {
        let user_id = user_id.clone();
        let created = now();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    token_hash,
                    user_id.as_str(),
                    created,
                    created + SESSION_TTL_MS
                ],
            )?;
            // Opportunistic cleanup; keeps the table from growing without bound
            // without needing a background job.
            conn.execute(
                "DELETE FROM sessions WHERE expires_at < ?1",
                params![created],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn user_for_session(&self, token_hash: String) -> Result<Option<User>> {
        let now_ms = now();
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT u.* FROM users u
                     JOIN sessions s ON s.user_id = u.id
                     WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                    params![token_hash, now_ms],
                    row_to_user,
                )
                .optional()?)
        })
        .await
    }

    /// Replace a user's password hash and revoke every session except the one
    /// making the change.
    ///
    /// Revoking is the point: if the password is being changed because someone
    /// else knows it, leaving their sessions alive would defeat the exercise.
    pub async fn change_password(
        &self,
        user_id: &UserId,
        password_hash: String,
        keep_token_hash: String,
    ) -> Result<()> {
        let id = user_id.clone();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE users SET password_hash = ?2 WHERE id = ?1",
                params![id.as_str(), password_hash],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE user_id = ?1 AND token_hash != ?2",
                params![id.as_str(), keep_token_hash],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn delete_session(&self, token_hash: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                params![token_hash],
            )?;
            Ok(())
        })
        .await
    }

    // --- waves ----------------------------------------------------------

    /// Create a wave with its root wavelet and initial participant set, in one
    /// transaction so a wave can never exist without somewhere to talk.
    pub async fn create_wave(
        &self,
        creator: UserId,
        title: String,
        participants: Vec<UserId>,
    ) -> Result<(Wave, Wavelet)> {
        let ts = now();
        let wave = Wave {
            id: WaveId::new(),
            creator: creator.clone(),
            created_at: ts,
        };
        let wavelet = Wavelet {
            id: WaveletId::new(),
            wave_id: wave.id.clone(),
            kind: WaveletKind::Conversation,
            title,
            participants,
            anchor_blip: None,
            created_at: ts,
            last_modified: ts,
        };

        let (w, s) = (wave.clone(), wavelet.clone());
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO waves (id, creator, created_at) VALUES (?1, ?2, ?3)",
                params![w.id.as_str(), w.creator.as_str(), w.created_at],
            )?;
            insert_wavelet(&tx, &s)?;
            tx.commit()?;
            Ok(())
        })
        .await?;

        Ok((wave, wavelet))
    }

    pub async fn create_wavelet(&self, wavelet: Wavelet) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            insert_wavelet(&tx, &wavelet)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn wave(&self, wave_id: &WaveId) -> Result<Option<Wave>> {
        let id = wave_id.clone();
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, creator, created_at FROM waves WHERE id = ?1",
                    params![id.as_str()],
                    |r| {
                        Ok(Wave {
                            id: WaveId(r.get(0)?),
                            creator: UserId(r.get(1)?),
                            created_at: r.get(2)?,
                        })
                    },
                )
                .optional()?)
        })
        .await
    }

    /// Every wavelet of a wave, with participants resolved. Not filtered by
    /// viewer — callers apply visibility, because the server needs the full set
    /// to route ops.
    pub async fn wavelets_of_wave(&self, wave_id: &WaveId) -> Result<Vec<Wavelet>> {
        let id = wave_id.clone();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, wave_id, kind, title, anchor_blip, created_at, last_modified
                 FROM wavelets WHERE wave_id = ?1 ORDER BY created_at",
            )?;
            let mut wavelets: Vec<Wavelet> = stmt
                .query_map(params![id.as_str()], row_to_wavelet)?
                .collect::<Result<_, _>>()?;

            let mut members = conn.prepare(
                "SELECT p.wavelet_id, p.user_id FROM participants p
                 JOIN wavelets s ON s.id = p.wavelet_id
                 WHERE s.wave_id = ?1 ORDER BY p.added_at",
            )?;
            let rows = members.query_map(params![id.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (wavelet_id, user_id) = row?;
                if let Some(w) = wavelets.iter_mut().find(|w| w.id.as_str() == wavelet_id) {
                    w.participants.push(UserId(user_id));
                }
            }
            Ok(wavelets)
        })
        .await
    }

    pub async fn blips_of_wave(&self, wave_id: &WaveId) -> Result<Vec<Blip>> {
        let id = wave_id.clone();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, wavelet_id, wave_id, parent, seq, author, contributors,
                        created_at, last_modified, content, revision, deleted
                 FROM blips WHERE wave_id = ?1 AND deleted = 0 ORDER BY seq",
            )?;
            let blips = stmt
                .query_map(params![id.as_str()], row_to_blip)?
                .collect::<Result<_, _>>()?;
            Ok(blips)
        })
        .await
    }

    pub async fn set_title(&self, wavelet_id: &WaveletId, title: String) -> Result<()> {
        let id = wavelet_id.clone();
        let ts = now();
        self.run(move |conn| {
            conn.execute(
                "UPDATE wavelets SET title = ?2, last_modified = ?3 WHERE id = ?1",
                params![id.as_str(), title, ts],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn add_participant(&self, wavelet_id: &WaveletId, user_id: &UserId) -> Result<()> {
        let (w, u) = (wavelet_id.clone(), user_id.clone());
        let ts = now();
        self.run(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO participants (wavelet_id, user_id, added_at)
                 VALUES (?1, ?2, ?3)",
                params![w.as_str(), u.as_str(), ts],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove_participant(&self, wavelet_id: &WaveletId, user_id: &UserId) -> Result<()> {
        let (w, u) = (wavelet_id.clone(), user_id.clone());
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM participants WHERE wavelet_id = ?1 AND user_id = ?2",
                params![w.as_str(), u.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    // --- blips ----------------------------------------------------------

    pub async fn insert_blip(&self, blip: Blip) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO blips (id, wavelet_id, wave_id, parent, seq, author, contributors,
                                    created_at, last_modified, content, revision, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
                params![
                    blip.id.as_str(),
                    blip.wavelet_id.as_str(),
                    blip.wave_id.as_str(),
                    blip.parent.as_ref().map(|p| p.0.clone()),
                    blip.seq,
                    blip.author.as_str(),
                    serde_json::to_string(&blip.contributors)?,
                    blip.created_at,
                    blip.last_modified,
                    serde_json::to_string(&blip.content)?,
                    blip.revision,
                ],
            )?;
            index_blip(&tx, &blip)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Persist an applied op together with the resulting snapshot.
    ///
    /// Both writes happen in one transaction: if the process dies mid-commit,
    /// the log and the snapshot stay consistent with each other.
    /// The revision a previously-accepted op produced, if this client op id has
    /// already been applied to this blip.
    pub async fn revision_for_op(&self, blip_id: &BlipId, op_id: &str) -> Result<Option<u64>> {
        let (b, o) = (blip_id.clone(), op_id.to_string());
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT revision FROM ops WHERE blip_id = ?1 AND op_id = ?2",
                    params![b.as_str(), o],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .map(|r| r as u64))
        })
        .await
    }

    pub async fn commit_op(
        &self,
        blip: Blip,
        delta: Delta,
        author: UserId,
        timestamp: Timestamp,
        op_id: Option<String>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO ops (blip_id, revision, wave_id, author, timestamp, delta, op_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    blip.id.as_str(),
                    blip.revision,
                    blip.wave_id.as_str(),
                    author.as_str(),
                    timestamp,
                    serde_json::to_string(&delta)?,
                    op_id,
                ],
            )?;
            tx.execute(
                "UPDATE blips SET content = ?2, revision = ?3, last_modified = ?4,
                                  contributors = ?5
                 WHERE id = ?1",
                params![
                    blip.id.as_str(),
                    serde_json::to_string(&blip.content)?,
                    blip.revision,
                    blip.last_modified,
                    serde_json::to_string(&blip.contributors)?,
                ],
            )?;
            tx.execute(
                "UPDATE wavelets SET last_modified = ?2 WHERE id = ?1",
                params![blip.wavelet_id.as_str(), blip.last_modified],
            )?;
            index_blip(&tx, &blip)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn delete_blip(&self, blip_id: &BlipId) -> Result<()> {
        let id = blip_id.clone();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE blips SET deleted = 1 WHERE id = ?1",
                params![id.as_str()],
            )?;
            tx.execute(
                "DELETE FROM blip_search WHERE blip_id = ?1",
                params![id.as_str()],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    // --- inbox ----------------------------------------------------------

    /// Every wave the user participates in, as inbox rows.
    ///
    /// Built from four bulk queries rather than one per wave, so inbox load
    /// stays flat as the number of waves grows.
    pub async fn inbox(&self, user_id: &UserId) -> Result<Vec<WaveSummary>> {
        let uid = user_id.clone();
        let users = self.all_users().await?;

        self.run(move |conn| {
            let me = uid.as_str().to_string();

            // 1. Root wavelet of every wave the user can see.
            let mut stmt = conn.prepare(
                "SELECT s.wave_id, s.title, s.last_modified
                 FROM wavelets s
                 WHERE s.kind = 'conversation'
                   AND s.wave_id IN (
                       SELECT s2.wave_id FROM wavelets s2
                       JOIN participants p ON p.wavelet_id = s2.id
                       WHERE p.user_id = ?1)",
            )?;
            let mut summaries: Vec<WaveSummary> = stmt
                .query_map(params![me], |r| {
                    Ok(WaveSummary {
                        id: WaveId(r.get(0)?),
                        title: r.get(1)?,
                        participants: Vec::new(),
                        last_modified: r.get(2)?,
                        snippet: String::new(),
                        snippet_author: None,
                        blip_count: 0,
                        unread_count: 0,
                        flags: WaveFlags::default(),
                    })
                })?
                .collect::<Result<_, _>>()?;

            let index = |summaries: &mut Vec<WaveSummary>, id: &str| -> Option<usize> {
                summaries.iter().position(|s| s.id.as_str() == id)
            };

            // 2. Participants of each root wavelet.
            let mut stmt = conn.prepare(
                "SELECT s.wave_id, p.user_id FROM wavelets s
                 JOIN participants p ON p.wavelet_id = s.id
                 WHERE s.kind = 'conversation'
                   AND s.wave_id IN (
                       SELECT s2.wave_id FROM wavelets s2
                       JOIN participants p2 ON p2.wavelet_id = s2.id
                       WHERE p2.user_id = ?1)
                 ORDER BY p.added_at",
            )?;
            let rows = stmt.query_map(params![me], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (wave_id, user_id) = row?;
                if let Some(i) = index(&mut summaries, &wave_id) {
                    if let Some(u) = users.iter().find(|u| u.id.as_str() == user_id) {
                        summaries[i].participants.push(u.clone());
                    }
                }
            }

            // 3. Blip count and most recent blip per wave. SQLite's bare-column
            //    rule returns the row that produced MAX(last_modified), giving
            //    the count and the latest snippet from a single scan.
            let mut stmt = conn.prepare(
                "SELECT b.wave_id, COUNT(*), b.author, b.content, MAX(b.last_modified)
                 FROM blips b
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 WHERE b.deleted = 0
                 GROUP BY b.wave_id",
            )?;
            let rows = stmt.query_map(params![me], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            for row in rows {
                let (wave_id, count, author, content, last) = row?;
                if let Some(i) = index(&mut summaries, &wave_id) {
                    summaries[i].blip_count = count as usize;
                    summaries[i].snippet_author = Some(UserId(author));
                    summaries[i].last_modified = summaries[i].last_modified.max(last);
                    if let Ok(delta) = serde_json::from_str::<Delta>(&content) {
                        summaries[i].snippet = snippet_of(&delta, 140);
                    }
                }
            }

            // 4. Unread counts: a blip is unread when it has no read mark, or a
            //    mark older than its current revision.
            let mut stmt = conn.prepare(
                "SELECT b.wave_id, COUNT(*)
                 FROM blips b
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 LEFT JOIN read_marks r ON r.blip_id = b.id AND r.user_id = ?1
                 WHERE b.deleted = 0 AND (r.revision IS NULL OR r.revision < b.revision)
                 GROUP BY b.wave_id",
            )?;
            let rows = stmt.query_map(params![me], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (wave_id, count) = row?;
                if let Some(i) = index(&mut summaries, &wave_id) {
                    summaries[i].unread_count = count as usize;
                }
            }

            // 5. Per-user flags.
            let mut stmt =
                conn.prepare("SELECT wave_id, archived, muted FROM wave_flags WHERE user_id = ?1")?;
            let rows = stmt.query_map(params![me], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (wave_id, archived, muted) = row?;
                if let Some(i) = index(&mut summaries, &wave_id) {
                    summaries[i].flags = WaveFlags {
                        archived: archived != 0,
                        muted: muted != 0,
                    };
                }
            }

            summaries.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
            Ok(summaries)
        })
        .await
    }

    /// A single inbox row, for pushing incremental updates.
    ///
    /// Scoped to one wave on purpose. Deriving it from `inbox()` meant every
    /// keystroke-driven update rebuilt the recipient's entire inbox — a full
    /// user-table scan plus five queries over all their waves — once per
    /// participant, which is what exhausted the connection pool under load.
    pub async fn wave_summary(
        &self,
        user_id: &UserId,
        wave_id: &WaveId,
    ) -> Result<Option<WaveSummary>> {
        let (uid, wid) = (user_id.clone(), wave_id.clone());
        let users = self.all_users().await?;

        self.run(move |conn| {
            let (me, wave) = (uid.as_str(), wid.as_str());

            // Visibility and the row itself in one query: no root wavelet the
            // caller participates in means no row.
            let base = conn
                .query_row(
                    "SELECT s.title, s.last_modified FROM wavelets s
                     WHERE s.wave_id = ?2 AND s.kind = 'conversation'
                       AND EXISTS (SELECT 1 FROM wavelets s2
                                   JOIN participants p ON p.wavelet_id = s2.id
                                   WHERE s2.wave_id = ?2 AND p.user_id = ?1)",
                    params![me, wave],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((title, last_modified)) = base else {
                return Ok(None);
            };

            let mut summary = WaveSummary {
                id: wid.clone(),
                title,
                participants: Vec::new(),
                last_modified,
                snippet: String::new(),
                snippet_author: None,
                blip_count: 0,
                unread_count: 0,
                flags: WaveFlags::default(),
            };

            let mut stmt = conn.prepare(
                "SELECT p.user_id FROM wavelets s
                 JOIN participants p ON p.wavelet_id = s.id
                 WHERE s.wave_id = ?1 AND s.kind = 'conversation' ORDER BY p.added_at",
            )?;
            let rows = stmt.query_map(params![wave], |r| r.get::<_, String>(0))?;
            for row in rows {
                let id = row?;
                if let Some(u) = users.iter().find(|u| u.id.as_str() == id) {
                    summary.participants.push(u.clone());
                }
            }

            if let Some((count, author, content, last)) = conn
                .query_row(
                    "SELECT COUNT(*), b.author, b.content, MAX(b.last_modified) FROM blips b
                     JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                     WHERE b.wave_id = ?2 AND b.deleted = 0",
                    params![me, wave],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()?
                .filter(|(count, ..)| *count > 0)
            {
                summary.blip_count = count as usize;
                summary.snippet_author = author.map(UserId);
                if let Some(last) = last {
                    summary.last_modified = summary.last_modified.max(last);
                }
                if let Some(content) = content {
                    if let Ok(delta) = serde_json::from_str::<Delta>(&content) {
                        summary.snippet = snippet_of(&delta, 140);
                    }
                }
            }

            summary.unread_count = conn.query_row(
                "SELECT COUNT(*) FROM blips b
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 LEFT JOIN read_marks r ON r.blip_id = b.id AND r.user_id = ?1
                 WHERE b.wave_id = ?2 AND b.deleted = 0
                   AND (r.revision IS NULL OR r.revision < b.revision)",
                params![me, wave],
                |r| r.get::<_, i64>(0),
            )? as usize;

            if let Some((archived, muted)) = conn
                .query_row(
                    "SELECT archived, muted FROM wave_flags WHERE user_id = ?1 AND wave_id = ?2",
                    params![me, wave],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()?
            {
                summary.flags = WaveFlags {
                    archived: archived != 0,
                    muted: muted != 0,
                };
            }

            Ok(Some(summary))
        })
        .await
    }

    /// Everyone who can see any part of a wave.
    pub async fn wave_participants(&self, wave_id: &WaveId) -> Result<Vec<UserId>> {
        let id = wave_id.clone();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT p.user_id FROM participants p
                 JOIN wavelets s ON s.id = p.wavelet_id WHERE s.wave_id = ?1",
            )?;
            let rows = stmt.query_map(params![id.as_str()], |r| Ok(UserId(r.get(0)?)))?;
            let out = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(out)
        })
        .await
    }

    /// Is this user a participant in any wavelet of this wave?
    pub async fn is_participant(&self, user_id: &UserId, wave_id: &WaveId) -> Result<bool> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM wavelets s
                                JOIN participants p ON p.wavelet_id = s.id
                                WHERE s.wave_id = ?2 AND p.user_id = ?1)",
                params![u.as_str(), w.as_str()],
                |r| r.get::<_, i64>(0).map(|n| n != 0),
            )?)
        })
        .await
    }

    // --- read state and flags -------------------------------------------

    /// Mark every blip the user can see in a wave as read at its current revision.
    pub async fn mark_wave_read(&self, user_id: &UserId, wave_id: &WaveId) -> Result<()> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO read_marks (user_id, blip_id, revision)
                 SELECT ?1, b.id, b.revision FROM blips b
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 WHERE b.wave_id = ?2
                 ON CONFLICT(user_id, blip_id) DO UPDATE SET revision = excluded.revision",
                params![u.as_str(), w.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    /// Read marks for a wave, as `(blip_id, revision)`.
    pub async fn read_marks(
        &self,
        user_id: &UserId,
        wave_id: &WaveId,
    ) -> Result<Vec<(BlipId, u64)>> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT r.blip_id, r.revision FROM read_marks r
                 JOIN blips b ON b.id = r.blip_id
                 WHERE r.user_id = ?1 AND b.wave_id = ?2",
            )?;
            let rows = stmt.query_map(params![u.as_str(), w.as_str()], |r| {
                Ok((BlipId(r.get(0)?), r.get::<_, i64>(1)? as u64))
            })?;
            let marks = rows.collect::<Result<_, _>>()?;
            Ok(marks)
        })
        .await
    }

    pub async fn flags(&self, user_id: &UserId, wave_id: &WaveId) -> Result<WaveFlags> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT archived, muted FROM wave_flags WHERE user_id = ?1 AND wave_id = ?2",
                    params![u.as_str(), w.as_str()],
                    |r| {
                        Ok(WaveFlags {
                            archived: r.get::<_, i64>(0)? != 0,
                            muted: r.get::<_, i64>(1)? != 0,
                        })
                    },
                )
                .optional()?
                .unwrap_or_default())
        })
        .await
    }

    pub async fn set_flags(
        &self,
        user_id: &UserId,
        wave_id: &WaveId,
        flags: WaveFlags,
    ) -> Result<()> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO wave_flags (user_id, wave_id, archived, muted)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user_id, wave_id) DO UPDATE
                   SET archived = excluded.archived, muted = excluded.muted",
                params![
                    u.as_str(),
                    w.as_str(),
                    flags.archived as i64,
                    flags.muted as i64
                ],
            )?;
            Ok(())
        })
        .await
    }

    // --- playback and search --------------------------------------------

    /// Every op ever applied in a wave, oldest first, restricted to wavelets the
    /// viewer participates in.
    pub async fn playback(&self, user_id: &UserId, wave_id: &WaveId) -> Result<Vec<PlaybackFrame>> {
        let (u, w) = (user_id.clone(), wave_id.clone());
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT o.blip_id, o.revision, o.author, o.timestamp, o.delta
                 FROM ops o
                 JOIN blips b ON b.id = o.blip_id
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 WHERE o.wave_id = ?2 AND b.deleted = 0
                 ORDER BY o.timestamp, o.revision
                 LIMIT 20000",
            )?;
            let rows = stmt.query_map(params![u.as_str(), w.as_str()], |r| {
                Ok((
                    BlipId(r.get(0)?),
                    r.get::<_, i64>(1)? as u64,
                    UserId(r.get(2)?),
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;

            let mut frames = Vec::new();
            for row in rows {
                let (blip_id, revision, author, timestamp, delta) = row?;
                let delta: Delta = serde_json::from_str(&delta).unwrap_or_default();
                frames.push(PlaybackFrame {
                    created: revision == 1,
                    blip_id,
                    revision,
                    author,
                    timestamp,
                    delta,
                });
            }
            Ok(frames)
        })
        .await
    }

    /// Full-text search over blips the user can see.
    pub async fn search(&self, user_id: &UserId, query: &str) -> Result<Vec<SearchHit>> {
        let uid = user_id.clone();
        let fts = to_fts_query(query);
        if fts.is_empty() {
            return Ok(Vec::new());
        }
        self.run(move |conn| {
            // Delimit matches with control characters rather than markup:
            // `snippet()` does not escape the surrounding text, so emitting raw
            // <mark> here would splice a blip's contents into every searcher's
            // page as live HTML. `highlight_snippet` escapes first, then puts
            // the tags back.
            let mut stmt = conn.prepare(
                "SELECT f.blip_id, f.wave_id, s.title, b.author, b.last_modified,
                        snippet(blip_search, 2, char(2), char(3), '…', 24)
                 FROM blip_search f
                 JOIN blips b ON b.id = f.blip_id
                 JOIN wavelets s ON s.wave_id = f.wave_id AND s.kind = 'conversation'
                 JOIN participants p ON p.wavelet_id = b.wavelet_id AND p.user_id = ?1
                 WHERE blip_search MATCH ?2 AND b.deleted = 0
                 ORDER BY rank
                 LIMIT 60",
            )?;
            let rows = stmt.query_map(params![uid.as_str(), fts], |r| {
                Ok(SearchHit {
                    blip_id: BlipId(r.get(0)?),
                    wave_id: WaveId(r.get(1)?),
                    title: r.get(2)?,
                    author: UserId(r.get(3)?),
                    timestamp: r.get(4)?,
                    snippet: highlight_snippet(&r.get::<_, String>(5)?),
                })
            })?;
            let hits = rows.collect::<Result<_, _>>()?;
            Ok(hits)
        })
        .await
    }

    /// Resolve usernames to ids, reporting any that do not exist so the caller
    /// can tell the user rather than silently dropping them.
    pub async fn resolve_names(&self, names: Vec<String>) -> Result<(Vec<UserId>, Vec<String>)> {
        if names.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        self.run(move |conn| {
            let lowered: Vec<String> = names.iter().map(|n| n.trim().to_lowercase()).collect();
            let placeholders = lowered.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT id, name FROM users WHERE name IN ({placeholders})");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(lowered.iter()), |r| {
                Ok((UserId(r.get(0)?), r.get::<_, String>(1)?))
            })?;

            let mut found = Vec::new();
            let mut found_names = Vec::new();
            for row in rows {
                let (id, name) = row?;
                found.push(id);
                found_names.push(name);
            }
            let missing = lowered
                .into_iter()
                .filter(|n| !found_names.contains(n))
                .collect();
            Ok((found, missing))
        })
        .await
    }
}

/// Bring a database up to [`SCHEMA_VERSION`], or refuse to run against one this
/// build does not understand.
///
/// Refusing is deliberate. A server that starts happily against a schema it
/// cannot read reports success and then serves empty inboxes, which reads to
/// users as "my conversations are gone". Failing at startup with an actionable
/// message is far better than failing silently at query time.
fn migrate(conn: &mut Connection) -> Result<()> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);

    if version == 0 {
        // Either a fresh database, or one created before versioning existed.
        // Distinguish by looking for a table the very first schema had.
        let initialised: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'users'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;

        if initialised {
            // A pre-versioning database has the v1 layout. Verify that, then let
            // the steps below bring it forward exactly as they would any other
            // v1 database. Applying the *current* schema here would be wrong:
            // `CREATE TABLE IF NOT EXISTS` skips existing tables, so later
            // columns would never appear, and statements referencing them fail.
            adopt_legacy(conn)?;
        } else {
            conn.execute_batch(include_str!("schema.sql"))
                .context("failed to apply schema")?;
        }
        version = BASELINE_VERSION;
        conn.pragma_update(None, "user_version", version)?;
    }

    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "this database was written by a newer version of Gal (schema v{version}, \
             this build understands v{SCHEMA_VERSION}). Upgrade Gal, or restore a backup \
             taken before the upgrade."
        );
    }

    // Each step bumps `user_version` inside the same transaction as its DDL, so
    // an interrupted upgrade cannot half-apply. Fresh databases run these too —
    // that is what keeps a new database and an upgraded one byte-identical.
    if version == 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(
            "ALTER TABLE ops ADD COLUMN op_id TEXT;
             CREATE UNIQUE INDEX IF NOT EXISTS ops_op_id ON ops(blip_id, op_id)
                 WHERE op_id IS NOT NULL;",
        )?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
        version = 2;
        tracing::debug!("migrated database schema to v2");
    }

    if version < SCHEMA_VERSION {
        anyhow::bail!(
            "no migration available from schema v{version} to v{SCHEMA_VERSION}; \
             this build cannot upgrade this database"
        );
    }

    tracing::debug!(schema_version = version, "database ready");
    Ok(())
}

/// Verify that an unversioned database really does match the v1 layout before
/// stamping it, so a corrupt or foreign file is rejected rather than adopted.
fn adopt_legacy(conn: &Connection) -> Result<()> {
    for (table, column) in [
        ("blips", "deleted"),
        ("blips", "revision"),
        ("wave_flags", "archived"),
        ("read_marks", "revision"),
        ("ops", "delta"),
    ] {
        let present: bool = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|c| c == column);
        if !present {
            anyhow::bail!(
                "database has an unrecognised schema: table `{table}` is missing column \
                 `{column}`. It was probably written by a different or much older build; \
                 Gal will not modify it. Restore a backup or start from an empty database."
            );
        }
    }
    Ok(())
}

// --- row mapping --------------------------------------------------------

fn row_to_user(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: UserId(row.get("id")?),
        name: row.get("name")?,
        display_name: row.get("display_name")?,
        email: row.get("email")?,
        password_hash: row.get("password_hash")?,
        color: row.get::<_, i64>("color")? as u16,
        created_at: row.get("created_at")?,
    })
}

fn row_to_wavelet(row: &Row<'_>) -> rusqlite::Result<Wavelet> {
    let kind: String = row.get(2)?;
    Ok(Wavelet {
        id: WaveletId(row.get(0)?),
        wave_id: WaveId(row.get(1)?),
        kind: if kind == "privateReply" {
            WaveletKind::PrivateReply
        } else {
            WaveletKind::Conversation
        },
        title: row.get(3)?,
        anchor_blip: row.get::<_, Option<String>>(4)?.map(BlipId),
        created_at: row.get(5)?,
        last_modified: row.get(6)?,
        participants: Vec::new(),
    })
}

fn row_to_blip(row: &Row<'_>) -> rusqlite::Result<Blip> {
    let contributors: String = row.get(6)?;
    let content: String = row.get(9)?;
    Ok(Blip {
        id: BlipId(row.get(0)?),
        wavelet_id: WaveletId(row.get(1)?),
        wave_id: WaveId(row.get(2)?),
        parent: row.get::<_, Option<String>>(3)?.map(BlipId),
        seq: row.get(4)?,
        author: UserId(row.get(5)?),
        contributors: serde_json::from_str(&contributors).unwrap_or_default(),
        created_at: row.get(7)?,
        last_modified: row.get(8)?,
        content: serde_json::from_str(&content).unwrap_or_default(),
        revision: row.get::<_, i64>(10)? as u64,
        deleted: row.get::<_, i64>(11)? != 0,
    })
}

fn insert_wavelet(tx: &rusqlite::Transaction<'_>, wavelet: &Wavelet) -> Result<()> {
    let kind = match wavelet.kind {
        WaveletKind::Conversation => "conversation",
        WaveletKind::PrivateReply => "privateReply",
    };
    tx.execute(
        "INSERT INTO wavelets (id, wave_id, kind, title, anchor_blip, created_at, last_modified)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            wavelet.id.as_str(),
            wavelet.wave_id.as_str(),
            kind,
            wavelet.title,
            wavelet.anchor_blip.as_ref().map(|b| b.0.clone()),
            wavelet.created_at,
            wavelet.last_modified,
        ],
    )?;
    for user in &wavelet.participants {
        tx.execute(
            "INSERT OR IGNORE INTO participants (wavelet_id, user_id, added_at)
             VALUES (?1, ?2, ?3)",
            params![wavelet.id.as_str(), user.as_str(), wavelet.created_at],
        )?;
    }
    Ok(())
}

/// Refresh a blip's full-text entry. FTS5 has no upsert, so replace the row.
fn index_blip(tx: &rusqlite::Transaction<'_>, blip: &Blip) -> Result<()> {
    tx.execute(
        "DELETE FROM blip_search WHERE blip_id = ?1",
        params![blip.id.as_str()],
    )?;
    tx.execute(
        "INSERT INTO blip_search (blip_id, wave_id, body) VALUES (?1, ?2, ?3)",
        params![
            blip.id.as_str(),
            blip.wave_id.as_str(),
            blip.content.to_plain_text()
        ],
    )?;
    Ok(())
}

/// Turn user input into a safe FTS5 query.
///
/// FTS5's query syntax would otherwise let stray punctuation raise a parse
/// error — or let a user craft column filters. Each word is quoted as a literal
/// and the final one gets a prefix wildcard so results appear while typing.
fn to_fts_query(input: &str) -> String {
    let words: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect();
    match words.split_last() {
        None => String::new(),
        Some((last, rest)) => {
            let mut parts: Vec<String> = rest.to_vec();
            parts.push(format!("{last}*"));
            parts.join(" AND ")
        }
    }
}

/// Markers FTS5 wraps around matched terms. Control characters are used
/// because they cannot appear in a blip: the editor strips them from input, and
/// they carry no meaning as text.
const MATCH_START: char = '\u{2}';
const MATCH_END: char = '\u{3}';

/// Turn a raw FTS snippet into HTML that is safe to insert into the page.
///
/// The blip text is attacker-controlled — anyone in a wave can type anything —
/// so every character is escaped, and only the match markers are converted back
/// into real tags. The client inserts the result as HTML in order to show the
/// highlight, which is only sound because of this function.
fn highlight_snippet(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            MATCH_START => out.push_str("<mark>"),
            MATCH_END => out.push_str("</mark>"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// First meaningful line of a delta, for inbox snippets.
fn snippet_of(delta: &Delta, max_chars: usize) -> String {
    let text = delta.to_plain_text();
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max_chars).collect();
        format!("{}…", cut.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A storage instance backed by a throwaway file, so the pool's connections
    /// all see the same database.
    async fn temp_storage() -> (Storage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path().join("test.db")).unwrap();
        (storage, dir)
    }

    async fn make_user(storage: &Storage, name: &str) -> User {
        storage
            .create_user(
                name.to_string(),
                name.to_string(),
                format!("{name}@example.com"),
                "hash".into(),
            )
            .await
            .unwrap()
    }

    /// Build a database shaped like the pre-versioning release: the v1 layout
    /// with no `user_version` stamp.
    fn write_legacy_database(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                display_name TEXT NOT NULL, email TEXT NOT NULL, password_hash TEXT NOT NULL,
                color INTEGER NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE sessions (token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL,
                created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
             CREATE TABLE waves (id TEXT PRIMARY KEY, creator TEXT NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE wavelets (id TEXT PRIMARY KEY, wave_id TEXT NOT NULL, kind TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '', anchor_blip TEXT, created_at INTEGER NOT NULL,
                last_modified INTEGER NOT NULL);
             CREATE TABLE participants (wavelet_id TEXT NOT NULL, user_id TEXT NOT NULL,
                added_at INTEGER NOT NULL, PRIMARY KEY (wavelet_id, user_id));
             CREATE TABLE blips (id TEXT PRIMARY KEY, wavelet_id TEXT NOT NULL, wave_id TEXT NOT NULL,
                parent TEXT, seq INTEGER NOT NULL, author TEXT NOT NULL, contributors TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_modified INTEGER NOT NULL, content TEXT NOT NULL,
                revision INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE wave_flags (user_id TEXT NOT NULL, wave_id TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0, muted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, wave_id));
             CREATE TABLE read_marks (user_id TEXT NOT NULL, blip_id TEXT NOT NULL,
                revision INTEGER NOT NULL, PRIMARY KEY (user_id, blip_id));
             CREATE TABLE ops (blip_id TEXT NOT NULL, revision INTEGER NOT NULL, wave_id TEXT NOT NULL,
                author TEXT NOT NULL, timestamp INTEGER NOT NULL, delta TEXT NOT NULL,
                PRIMARY KEY (blip_id, revision));
             CREATE VIRTUAL TABLE blip_search USING fts5(blip_id UNINDEXED, wave_id UNINDEXED, body);
             PRAGMA user_version = 0;",
        )
        .unwrap();
    }

    fn schema_version(path: &std::path::Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn has_column(path: &std::path::Path, table: &str, column: &str) -> bool {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        cols.iter().any(|c| c == column)
    }

    #[tokio::test]
    async fn a_pre_versioning_database_is_migrated_not_mis_stamped() {
        // Regression: adopt_legacy used to stamp such a database with the
        // *current* version, so every migration step was skipped. Since
        // `CREATE TABLE IF NOT EXISTS` cannot add a column to an existing table,
        // the database ended up claiming to be current while missing `ops.op_id`
        // — and the server failed at the first edit.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        write_legacy_database(&path);
        assert_eq!(schema_version(&path), 0);
        assert!(!has_column(&path, "ops", "op_id"));

        let storage = Storage::open(&path).unwrap();

        assert_eq!(
            schema_version(&path),
            SCHEMA_VERSION,
            "should be brought fully forward"
        );
        assert!(
            has_column(&path, "ops", "op_id"),
            "the v1->v2 step must have run"
        );

        // And it actually works: a full create-and-edit round trip.
        let alice = make_user(&storage, "alice").await;
        let (wave, wavelet) = storage
            .create_wave(
                alice.id.clone(),
                "After upgrade".into(),
                vec![alice.id.clone()],
            )
            .await
            .unwrap();
        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("still works");
        blip.revision = 1;
        storage.insert_blip(blip.clone()).await.unwrap();
        storage
            .commit_op(
                blip,
                Delta::new(),
                alice.id.clone(),
                now(),
                Some("op-1".into()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_fresh_database_and_an_upgraded_one_agree() {
        // schema.sql is a frozen v1 baseline and every later change is a
        // migration, so both paths must converge on the same layout.
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh.db");
        let upgraded = dir.path().join("upgraded.db");
        let _ = Storage::open(&fresh).unwrap();
        write_legacy_database(&upgraded);
        let _ = Storage::open(&upgraded).unwrap();

        assert_eq!(schema_version(&fresh), schema_version(&upgraded));
        for (table, column) in [("ops", "op_id"), ("blips", "deleted")] {
            assert_eq!(
                has_column(&fresh, table, column),
                has_column(&upgraded, table, column),
                "{table}.{column} differs between a fresh and an upgraded database"
            );
        }
    }

    #[tokio::test]
    async fn a_database_that_is_not_recognisably_gal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.db");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT);")
            .unwrap();
        let err = match Storage::open(&path) {
            Ok(_) => panic!("a foreign database was adopted instead of refused"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("unrecognised schema"),
            "expected a clear refusal, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn users_round_trip_and_names_are_unique() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;

        let found = storage.user_by_name("alice").await.unwrap().unwrap();
        assert_eq!(found.id, alice.id);
        // Lookup is case-insensitive on the handle.
        assert!(storage.user_by_name("ALICE").await.unwrap().is_some());
        assert!(storage.user_by_name("nobody").await.unwrap().is_none());

        let duplicate = storage
            .create_user(
                "alice".into(),
                "Alice II".into(),
                "a2@x.com".into(),
                "h".into(),
            )
            .await;
        assert!(duplicate.is_err(), "duplicate handles must be rejected");
    }

    #[tokio::test]
    async fn sessions_expire_and_can_be_revoked() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;

        storage
            .create_session(&alice.id, "hash-1".into())
            .await
            .unwrap();
        let found = storage.user_for_session("hash-1".into()).await.unwrap();
        assert_eq!(found.unwrap().id, alice.id);

        storage.delete_session("hash-1".into()).await.unwrap();
        assert!(storage
            .user_for_session("hash-1".into())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn creating_a_wave_sets_up_a_root_wavelet() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let bob = make_user(&storage, "bob").await;

        let (wave, wavelet) = storage
            .create_wave(
                alice.id.clone(),
                "Launch".into(),
                vec![alice.id.clone(), bob.id.clone()],
            )
            .await
            .unwrap();

        let wavelets = storage.wavelets_of_wave(&wave.id).await.unwrap();
        assert_eq!(wavelets.len(), 1);
        assert_eq!(wavelets[0].id, wavelet.id);
        assert_eq!(wavelets[0].kind, WaveletKind::Conversation);
        assert_eq!(wavelets[0].participants.len(), 2);
    }

    #[tokio::test]
    async fn inbox_reports_snippet_counts_and_unread() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let bob = make_user(&storage, "bob").await;
        let (wave, wavelet) = storage
            .create_wave(
                alice.id.clone(),
                "Launch".into(),
                vec![alice.id.clone(), bob.id.clone()],
            )
            .await
            .unwrap();

        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("Ship it on Friday");
        blip.revision = 1;
        storage.insert_blip(blip.clone()).await.unwrap();

        let inbox = storage.inbox(&bob.id).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].title, "Launch");
        assert_eq!(inbox[0].snippet, "Ship it on Friday");
        assert_eq!(inbox[0].blip_count, 1);
        assert_eq!(inbox[0].unread_count, 1, "bob has not read it");
        assert_eq!(inbox[0].participants.len(), 2);

        storage.mark_wave_read(&bob.id, &wave.id).await.unwrap();
        let inbox = storage.inbox(&bob.id).await.unwrap();
        assert_eq!(inbox[0].unread_count, 0);
    }

    #[tokio::test]
    async fn editing_a_blip_makes_it_unread_again() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let bob = make_user(&storage, "bob").await;
        let (wave, wavelet) = storage
            .create_wave(
                alice.id.clone(),
                "T".into(),
                vec![alice.id.clone(), bob.id.clone()],
            )
            .await
            .unwrap();

        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("draft");
        blip.revision = 1;
        storage.insert_blip(blip.clone()).await.unwrap();
        storage.mark_wave_read(&bob.id, &wave.id).await.unwrap();
        assert_eq!(storage.inbox(&bob.id).await.unwrap()[0].unread_count, 0);

        // Alice revises it.
        blip.content = Delta::document("final");
        blip.revision = 2;
        storage
            .commit_op(
                blip.clone(),
                Delta::new().retain(5),
                alice.id.clone(),
                now(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            storage.inbox(&bob.id).await.unwrap()[0].unread_count,
            1,
            "a revision past the read mark should resurface the blip"
        );
    }

    #[tokio::test]
    async fn private_replies_are_invisible_to_outsiders() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let bob = make_user(&storage, "bob").await;
        let carol = make_user(&storage, "carol").await;

        let (wave, root) = storage
            .create_wave(
                alice.id.clone(),
                "Team".into(),
                vec![alice.id.clone(), bob.id.clone(), carol.id.clone()],
            )
            .await
            .unwrap();

        // Alice and Bob branch off privately.
        let private = Wavelet {
            id: WaveletId::new(),
            wave_id: wave.id.clone(),
            kind: WaveletKind::PrivateReply,
            title: "aside".into(),
            participants: vec![alice.id.clone(), bob.id.clone()],
            anchor_blip: None,
            created_at: now(),
            last_modified: now(),
        };
        storage.create_wavelet(private.clone()).await.unwrap();

        let mut secret = Blip::new(
            wave.id.clone(),
            private.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        secret.content = Delta::document("between us: the date slips");
        secret.revision = 1;
        storage.insert_blip(secret).await.unwrap();

        let mut public = Blip::new(wave.id.clone(), root.id.clone(), alice.id.clone(), None, 1);
        public.content = Delta::document("on track");
        public.revision = 1;
        storage.insert_blip(public).await.unwrap();

        // Carol sees only the public blip.
        assert_eq!(storage.inbox(&carol.id).await.unwrap()[0].blip_count, 1);
        assert_eq!(storage.inbox(&bob.id).await.unwrap()[0].blip_count, 2);

        // And it does not surface in her search.
        let hits = storage.search(&carol.id, "between us").await.unwrap();
        assert!(hits.is_empty(), "carol must not find a private reply");
        let hits = storage.search(&bob.id, "between us").await.unwrap();
        assert_eq!(hits.len(), 1, "bob is a participant and should find it");
    }

    #[tokio::test]
    async fn search_finds_edited_text_and_ignores_deleted_blips() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let (wave, wavelet) = storage
            .create_wave(alice.id.clone(), "Notes".into(), vec![alice.id.clone()])
            .await
            .unwrap();

        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("original wording");
        blip.revision = 1;
        storage.insert_blip(blip.clone()).await.unwrap();
        assert_eq!(
            storage.search(&alice.id, "original").await.unwrap().len(),
            1
        );

        // Rewrite it; the index must follow.
        blip.content = Delta::document("replacement phrasing");
        blip.revision = 2;
        storage
            .commit_op(blip.clone(), Delta::new(), alice.id.clone(), now(), None)
            .await
            .unwrap();
        assert!(storage
            .search(&alice.id, "original")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .search(&alice.id, "replacement")
                .await
                .unwrap()
                .len(),
            1
        );

        storage.delete_blip(&blip.id).await.unwrap();
        assert!(storage
            .search(&alice.id, "replacement")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn search_input_with_punctuation_does_not_error() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        // FTS5 would reject these as raw queries.
        for query in ["\"", "AND", "a OR (", "*", "-", "NEAR(", ""] {
            let result = storage.search(&alice.id, query).await;
            assert!(
                result.is_ok(),
                "query {query:?} should be sanitised, got {result:?}"
            );
        }
    }

    #[test]
    fn snippet_highlighting_escapes_markup_but_keeps_the_mark() {
        let raw = format!("a {MATCH_START}hit{MATCH_END} <img src=x onerror=alert(1)>");
        let safe = highlight_snippet(&raw);
        assert_eq!(
            safe,
            "a <mark>hit</mark> &lt;img src=x onerror=alert(1)&gt;",
        );
        assert!(!safe.contains("<img"), "markup must not survive");
    }

    #[tokio::test]
    async fn search_snippets_cannot_inject_html() {
        // A blip's text is attacker-controlled. FTS5's snippet() does not escape
        // it, so without escaping this becomes live markup in every searcher's
        // page.
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let (wave, wavelet) = storage
            .create_wave(alice.id.clone(), "P".into(), vec![alice.id.clone()])
            .await
            .unwrap();

        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("pineapple <img src=x onerror=\"steal()\"> tag");
        blip.revision = 1;
        storage.insert_blip(blip).await.unwrap();

        let hits = storage.search(&alice.id, "pineapple").await.unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(
            snippet.contains("<mark>"),
            "the match should still be highlighted"
        );
        assert!(
            !snippet.contains("<img"),
            "raw markup leaked into a snippet: {snippet}"
        );
        assert!(
            !snippet.contains("onerror=\""),
            "an event handler survived: {snippet}"
        );
        assert!(
            snippet.contains("&lt;img"),
            "markup should be escaped: {snippet}"
        );
    }

    #[tokio::test]
    async fn search_matches_prefixes_for_type_ahead() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let (wave, wavelet) = storage
            .create_wave(alice.id.clone(), "N".into(), vec![alice.id.clone()])
            .await
            .unwrap();
        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        blip.content = Delta::document("deployment checklist");
        blip.revision = 1;
        storage.insert_blip(blip).await.unwrap();

        assert_eq!(storage.search(&alice.id, "deploy").await.unwrap().len(), 1);
        assert_eq!(
            storage
                .search(&alice.id, "deployment check")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(storage
            .search(&alice.id, "rollback")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn playback_returns_ops_in_order() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let (wave, wavelet) = storage
            .create_wave(alice.id.clone(), "H".into(), vec![alice.id.clone()])
            .await
            .unwrap();

        let mut blip = Blip::new(
            wave.id.clone(),
            wavelet.id.clone(),
            alice.id.clone(),
            None,
            0,
        );
        storage.insert_blip(blip.clone()).await.unwrap();

        for (i, text) in ["Hello", " world", "!"].iter().enumerate() {
            blip.revision = i as u64 + 1;
            blip.content = blip
                .content
                .apply(&Delta::new().retain(blip.content.len()).insert(*text));
            storage
                .commit_op(
                    blip.clone(),
                    Delta::new()
                        .retain(blip.content.len() - text.len())
                        .insert(*text),
                    alice.id.clone(),
                    now() + i as i64,
                    None,
                )
                .await
                .unwrap();
        }

        let frames = storage.playback(&alice.id, &wave.id).await.unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames.iter().map(|f| f.revision).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(frames[0].created, "first revision marks blip creation");

        // Replaying the frames reproduces the final document.
        let deltas: Vec<Delta> = frames.iter().map(|f| f.delta.clone()).collect();
        assert_eq!(
            gal_ot::replay(&deltas, deltas.len()).to_plain_text(),
            "Hello world!"
        );
    }

    #[tokio::test]
    async fn resolve_names_reports_unknown_users() {
        let (storage, _dir) = temp_storage().await;
        make_user(&storage, "alice").await;
        make_user(&storage, "bob").await;

        let (found, missing) = storage
            .resolve_names(vec!["alice".into(), "Bob".into(), "nobody".into()])
            .await
            .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(missing, vec!["nobody".to_string()]);
    }

    #[tokio::test]
    async fn flags_default_to_false_and_persist() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let (wave, _) = storage
            .create_wave(alice.id.clone(), "T".into(), vec![alice.id.clone()])
            .await
            .unwrap();

        assert!(!storage.flags(&alice.id, &wave.id).await.unwrap().archived);
        storage
            .set_flags(
                &alice.id,
                &wave.id,
                WaveFlags {
                    archived: true,
                    muted: false,
                },
            )
            .await
            .unwrap();
        assert!(storage.flags(&alice.id, &wave.id).await.unwrap().archived);
        assert!(storage.inbox(&alice.id).await.unwrap()[0].flags.archived);
    }

    #[tokio::test]
    async fn removing_a_participant_hides_the_wave() {
        let (storage, _dir) = temp_storage().await;
        let alice = make_user(&storage, "alice").await;
        let bob = make_user(&storage, "bob").await;
        let (wave, wavelet) = storage
            .create_wave(
                alice.id.clone(),
                "T".into(),
                vec![alice.id.clone(), bob.id.clone()],
            )
            .await
            .unwrap();

        assert_eq!(storage.inbox(&bob.id).await.unwrap().len(), 1);
        storage
            .remove_participant(&wavelet.id, &bob.id)
            .await
            .unwrap();
        assert!(storage.inbox(&bob.id).await.unwrap().is_empty());
        // Alice, who is still a participant, keeps her view of the wave.
        let alice_inbox = storage.inbox(&alice.id).await.unwrap();
        assert_eq!(alice_inbox.len(), 1);
        assert_eq!(alice_inbox[0].id, wave.id);
    }
}
