//! 本地存储：SQLite + 字段级 AEAD 加密，主密钥托管在 OS Keychain。对应 §4.6、§5。
//!
//! **与方案 §3 的一处偏差（已知并记录）**：原计划用 SQLCipher 做整库加密。
//! `rusqlite` 的 `bundled-sqlcipher-vendored-openssl` 需要从源码编译 OpenSSL，
//! 在本机 Windows 工具链上因 Perl 模块缺失无法构建。MVP 改为
//! **SQLite + 对敏感列做 AES-256-GCM 字段级加密**，密钥同样存 Windows Credential Manager。
//!
//! 安全性差异：库结构与时间戳等元数据不再加密，逐字稿/笔记/纪要正文仍然加密。
//! 对 MVP 内部试用可接受；v1.0 应改回整库加密（补 OpenSSL 工具链或换 SQLCipher 预编译库）。

use std::path::Path;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};

use crate::policy::EgressRecord;
use crate::{Error, Result, Source, Utterance};

const KEYCHAIN_SERVICE: &str = "VocMeet";
const KEYCHAIN_DB_KEY: &str = "db-master-key";
const KEYCHAIN_API_KEY: &str = "llm-api-key";

/// 取出（必要时生成）数据库主密钥。密钥只存在于 Keychain，DB 文件里没有密钥材料。
pub fn master_key() -> Result<Vec<u8>> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_DB_KEY)
        .map_err(|e| Error::Keychain(format!("open entry: {e}")))?;

    match entry.get_password() {
        Ok(existing) => B64
            .decode(existing.as_bytes())
            .map_err(|e| Error::Keychain(format!("decode stored key: {e}"))),
        Err(_) => {
            let key = Aes256Gcm::generate_key(&mut OsRng);
            let encoded = B64.encode(key.as_slice());
            entry
                .set_password(&encoded)
                .map_err(|e| Error::Keychain(format!("store key: {e}")))?;
            Ok(key.to_vec())
        }
    }
}

/// LLM API Key 的读写。同样不落 DB、不落配置文件。
pub fn set_api_key(value: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_API_KEY)
        .map_err(|e| Error::Keychain(format!("open entry: {e}")))?;
    entry
        .set_password(value)
        .map_err(|e| Error::Keychain(format!("store api key: {e}")))
}

pub fn get_api_key() -> Option<String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_API_KEY)
        .ok()?
        .get_password()
        .ok()
}

/// 字段级加解密。
#[derive(Clone)]
pub struct Cipher {
    inner: Aes256Gcm,
}

impl Cipher {
    pub fn new(key_bytes: &[u8]) -> Result<Self> {
        if key_bytes.len() != 32 {
            return Err(Error::Crypto(format!(
                "expected 32-byte key, got {}",
                key_bytes.len()
            )));
        }
        let key = Key::<Aes256Gcm>::from_slice(key_bytes);
        Ok(Self {
            inner: Aes256Gcm::new(key),
        })
    }

    /// 输出 base64(nonce ‖ ciphertext)。每次加密都用新随机 nonce。
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .inner
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| Error::Crypto(format!("encrypt: {e}")))?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        Ok(B64.encode(blob))
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let blob = B64
            .decode(encoded.as_bytes())
            .map_err(|e| Error::Crypto(format!("decode: {e}")))?;
        if blob.len() < 12 {
            return Err(Error::Crypto("ciphertext too short".into()));
        }
        let (nonce_bytes, ct) = blob.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let pt = self
            .inner
            .decrypt(nonce, ct)
            .map_err(|e| Error::Crypto(format!("decrypt (密钥不匹配或数据损坏): {e}")))?;
        String::from_utf8(pt).map_err(|e| Error::Crypto(format!("utf8: {e}")))
    }
}

pub struct Store {
    conn: Connection,
    cipher: Cipher,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meeting {
    pub id: i64,
    pub title: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: i64,
    pub status: String,
    #[serde(default)]
    pub archived_at: Option<String>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, key: &[u8]) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        let store = Self {
            conn,
            cipher: Cipher::new(key)?,
        };
        store.migrate()?;
        Ok(store)
    }

    /// 内存库，供测试使用。
    pub fn open_in_memory(key: &[u8]) -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
            cipher: Cipher::new(key)?,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS meetings (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                title       TEXT NOT NULL,
                started_at  TEXT NOT NULL,
                ended_at    TEXT,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                status      TEXT NOT NULL DEFAULT 'recording',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                archived_at TEXT
            );

            CREATE TABLE IF NOT EXISTS audio_chunks (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id  INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                source      TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                path        TEXT NOT NULL,
                start_ms    INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                sha256      TEXT
            );

            -- text 列为密文（base64(nonce||ct)）
            CREATE TABLE IF NOT EXISTS utterances (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id      INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                ordinal         INTEGER NOT NULL,
                speaker_id      TEXT NOT NULL,
                start_ms        INTEGER NOT NULL,
                end_ms          INTEGER NOT NULL,
                text            TEXT NOT NULL,
                source          TEXT NOT NULL,
                low_confidence  INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS speakers (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id   INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                speaker_key  TEXT NOT NULL,
                display_name TEXT NOT NULL,
                UNIQUE(meeting_id, speaker_key)
            );

            CREATE TABLE IF NOT EXISTS notes (
                meeting_id INTEGER PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
                content    TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS summaries (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id  INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                model       TEXT NOT NULL,
                content     TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS jobs (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
                kind       TEXT NOT NULL,
                state      TEXT NOT NULL,
                progress   REAL NOT NULL DEFAULT 0,
                error      TEXT,
                attempts   INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            -- 只记元数据，不记内容。见 §5。
            CREATE TABLE IF NOT EXISTS egress_log (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id INTEGER,
                endpoint   TEXT NOT NULL,
                model      TEXT NOT NULL,
                purpose    TEXT NOT NULL,
                chars_sent INTEGER NOT NULL,
                redacted   INTEGER NOT NULL,
                policy     TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            -- 会议级派生数据：主题关键词与合并后的回放音轨、逐字稿文件落盘路径。
            CREATE TABLE IF NOT EXISTS meeting_meta (
                meeting_id      INTEGER PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
                keywords        TEXT NOT NULL DEFAULT '[]',
                playback_path   TEXT,
                transcript_path TEXT,
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_utt_meeting ON utterances(meeting_id, ordinal);
            CREATE INDEX IF NOT EXISTS idx_chunk_meeting ON audio_chunks(meeting_id, source, seq);
            "#,
        )?;
        let _ = self.conn.execute("ALTER TABLE meetings ADD COLUMN archived_at TEXT", []);
        let _ = self.conn.execute("ALTER TABLE meeting_meta ADD COLUMN transcript_path TEXT", []);
        Ok(())
    }

    // ---------- meetings ----------

    pub fn create_meeting(&self, title: &str, started_at: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO meetings (title, started_at, status) VALUES (?1, ?2, 'recording')",
            params![title, started_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_meeting(&self, meeting_id: i64, ended_at: &str, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE meetings SET ended_at = ?2, duration_ms = ?3, status = 'recorded' WHERE id = ?1",
            params![meeting_id, ended_at, duration_ms],
        )?;
        Ok(())
    }

    pub fn set_meeting_status(&self, meeting_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE meetings SET status = ?2 WHERE id = ?1",
            params![meeting_id, status],
        )?;
        Ok(())
    }

    /// 删除会议及其全部关联数据（外键 ON DELETE CASCADE）。
    pub fn delete_meeting(&self, meeting_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM meetings WHERE id = ?1", params![meeting_id])?;
        Ok(())
    }

    pub fn list_meetings(&self) -> Result<Vec<Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, started_at, ended_at, duration_ms, status, archived_at
             FROM meetings WHERE archived_at IS NULL ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Meeting {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    duration_ms: r.get(4)?,
                    status: r.get(5)?,
                    archived_at: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 分页列出会议（未归档）。会议数量会随使用增长，侧栏不能一次性全加载。
    pub fn list_meetings_page(&self, offset: i64, limit: i64) -> Result<Vec<Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, started_at, ended_at, duration_ms, status, archived_at
             FROM meetings WHERE archived_at IS NULL ORDER BY id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt
            .query_map(params![limit, offset], |r| {
                Ok(Meeting {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    duration_ms: r.get(4)?,
                    status: r.get(5)?,
                    archived_at: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count_meetings(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM meetings WHERE archived_at IS NULL", [], |r| r.get(0))?)
    }

    /// 列出所有已归档会议。
    pub fn list_archived_meetings(&self) -> Result<Vec<Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, started_at, ended_at, duration_ms, status, archived_at
             FROM meetings WHERE archived_at IS NOT NULL ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Meeting {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    duration_ms: r.get(4)?,
                    status: r.get(5)?,
                    archived_at: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count_archived_meetings(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM meetings WHERE archived_at IS NOT NULL", [], |r| r.get(0))?)
    }

    /// 归档或取消归档会议（软删除）。
    pub fn archive_meeting(&self, meeting_id: i64, archived: bool) -> Result<()> {
        if archived {
            self.conn.execute(
                "UPDATE meetings SET archived_at = datetime('now') WHERE id = ?1",
                params![meeting_id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE meetings SET archived_at = NULL WHERE id = ?1",
                params![meeting_id],
            )?;
        }
        Ok(())
    }

    /// 保存关键词（明文——它们是派生的主题词，不含原始发言内容）。
    pub fn set_keywords(&self, meeting_id: i64, keywords: &[String]) -> Result<()> {
        let json = serde_json::to_string(keywords)
            .map_err(|e| Error::Config(format!("serialize keywords: {e}")))?;
        self.conn.execute(
            "INSERT INTO meeting_meta (meeting_id, keywords, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(meeting_id) DO UPDATE SET keywords = excluded.keywords, updated_at = datetime('now')",
            params![meeting_id, json],
        )?;
        Ok(())
    }

    pub fn get_keywords(&self, meeting_id: i64) -> Result<Vec<String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT keywords FROM meeting_meta WHERE meeting_id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(j) => Ok(serde_json::from_str(&j).unwrap_or_default()),
            None => Ok(Vec::new()),
        }
    }

    /// 合并后的回放音轨路径。
    pub fn set_playback_path(&self, meeting_id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meeting_meta (meeting_id, playback_path, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(meeting_id) DO UPDATE SET playback_path = excluded.playback_path, updated_at = datetime('now')",
            params![meeting_id, path],
        )?;
        Ok(())
    }

    pub fn get_playback_path(&self, meeting_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT playback_path FROM meeting_meta WHERE meeting_id = ?1",
                params![meeting_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// 逐字稿生成文件的绝对路径。
    pub fn set_transcript_path(&self, meeting_id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meeting_meta (meeting_id, transcript_path, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(meeting_id) DO UPDATE SET transcript_path = excluded.transcript_path, updated_at = datetime('now')",
            params![meeting_id, path],
        )?;
        Ok(())
    }

    pub fn get_transcript_path(&self, meeting_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT transcript_path FROM meeting_meta WHERE meeting_id = ?1",
                params![meeting_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn get_meeting(&self, meeting_id: i64) -> Result<Option<Meeting>> {
        let m = self
            .conn
            .query_row(
                "SELECT id, title, started_at, ended_at, duration_ms, status, archived_at FROM meetings WHERE id = ?1",
                params![meeting_id],
                |r| {
                    Ok(Meeting {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        started_at: r.get(2)?,
                        ended_at: r.get(3)?,
                        duration_ms: r.get(4)?,
                        status: r.get(5)?,
                        archived_at: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(m)
    }

    // ---------- utterances ----------

    /// 覆盖式写入整场逐字稿（转写完成或重新转写时调用）。
    pub fn replace_utterances(&self, meeting_id: i64, utterances: &[Utterance]) -> Result<()> {
        self.conn.execute(
            "DELETE FROM utterances WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO utterances
             (meeting_id, ordinal, speaker_id, start_ms, end_ms, text, source, low_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for u in utterances {
            stmt.execute(params![
                meeting_id,
                u.id,
                u.speaker_id,
                u.start_ms,
                u.end_ms,
                self.cipher.encrypt(&u.text)?,
                u.source.as_str(),
                u.low_confidence as i32,
            ])?;
        }
        Ok(())
    }

    /// 读出逐字稿，并把说话人真名（若已命名）回填到 `speaker_name`。
    pub fn load_utterances(&self, meeting_id: i64) -> Result<Vec<Utterance>> {
        let names = self.load_speaker_names(meeting_id)?;

        let mut stmt = self.conn.prepare(
            "SELECT ordinal, speaker_id, start_ms, end_ms, text, source, low_confidence
             FROM utterances WHERE meeting_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![meeting_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (ordinal, speaker_id, start_ms, end_ms, cipher_text, source, low) = row?;
            out.push(Utterance {
                id: ordinal,
                speaker_name: names.get(&speaker_id).cloned(),
                speaker_id,
                start_ms: start_ms as u32,
                end_ms: end_ms as u32,
                text: self.cipher.decrypt(&cipher_text)?,
                source: Source::parse(&source).unwrap_or(Source::System),
                low_confidence: low != 0,
            });
        }
        Ok(out)
    }

    /// 校对后更新单条发言正文。
    pub fn update_utterance_text(&self, meeting_id: i64, ordinal: i64, text: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE utterances SET text = ?3 WHERE meeting_id = ?1 AND ordinal = ?2",
            params![meeting_id, ordinal, self.cipher.encrypt(text)?],
        )?;
        Ok(())
    }

    // ---------- speakers ----------

    pub fn name_speaker(&self, meeting_id: i64, speaker_key: &str, display_name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO speakers (meeting_id, speaker_key, display_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(meeting_id, speaker_key) DO UPDATE SET display_name = excluded.display_name",
            params![meeting_id, speaker_key, display_name],
        )?;
        Ok(())
    }

    pub fn load_speaker_names(
        &self,
        meeting_id: i64,
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT speaker_key, display_name FROM speakers WHERE meeting_id = ?1")?;
        let rows = stmt.query_map(params![meeting_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// 逐字稿里出现过的说话人 key（去重，按首次出现排序）。UI 命名界面用。
    pub fn distinct_speakers(&self, meeting_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT speaker_id, MIN(ordinal) AS first_seen FROM utterances
             WHERE meeting_id = ?1 GROUP BY speaker_id ORDER BY first_seen",
        )?;
        let rows = stmt.query_map(params![meeting_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---------- notes / summaries ----------

    pub fn save_note(&self, meeting_id: i64, content: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO notes (meeting_id, content, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(meeting_id) DO UPDATE SET content = excluded.content, updated_at = datetime('now')",
            params![meeting_id, self.cipher.encrypt(content)?],
        )?;
        Ok(())
    }

    pub fn load_note(&self, meeting_id: i64) -> Result<String> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT content FROM notes WHERE meeting_id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(c) => self.cipher.decrypt(&c),
            None => Ok(String::new()),
        }
    }

    pub fn save_summary(&self, meeting_id: i64, model: &str, content: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO summaries (meeting_id, model, content) VALUES (?1, ?2, ?3)",
            params![meeting_id, model, self.cipher.encrypt(content)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn latest_summary(&self, meeting_id: i64) -> Result<Option<String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT content FROM summaries WHERE meeting_id = ?1 ORDER BY id DESC LIMIT 1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(c) => Ok(Some(self.cipher.decrypt(&c)?)),
            None => Ok(None),
        }
    }

    // ---------- audit ----------

    pub fn log_egress(&self, meeting_id: Option<i64>, rec: &EgressRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO egress_log (meeting_id, endpoint, model, purpose, chars_sent, redacted, policy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                meeting_id,
                rec.endpoint,
                rec.model,
                rec.purpose,
                rec.chars_sent as i64,
                rec.redacted as i32,
                rec.policy,
            ],
        )?;
        Ok(())
    }

    pub fn egress_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM egress_log", [], |r| r.get(0))?)
    }

    /// 聚合全局所有会议中出现过的参会人。
    pub fn list_all_participants(&self) -> Result<Vec<ParticipantInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.speaker_id,
                    COUNT(DISTINCT u.meeting_id) as meeting_cnt,
                    COUNT(u.id) as utt_cnt,
                    MAX(m.started_at) as last_seen
             FROM utterances u
             JOIN meetings m ON u.meeting_id = m.id
             GROUP BY u.speaker_id
             ORDER BY last_seen DESC, utt_cnt DESC",
        )?;

        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, i64>(2)? as usize,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut list = Vec::new();
        for (key, meeting_cnt, utt_cnt, last_seen) in rows {
            let name_opt: Option<String> = self
                .conn
                .query_row(
                    "SELECT display_name FROM speakers WHERE speaker_key = ?1 ORDER BY id DESC LIMIT 1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;

            let sample = self
                .conn
                .query_row(
                    "SELECT text FROM utterances WHERE speaker_id = ?1 ORDER BY LENGTH(text) DESC LIMIT 1",
                    params![key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .and_then(|enc| self.cipher.decrypt(&enc).ok())
                .map(|t| t.chars().take(40).collect::<String>())
                .unwrap_or_default();

            list.push(ParticipantInfo {
                key,
                display_name: name_opt,
                meeting_count: meeting_cnt,
                utterance_count: utt_cnt,
                sample,
                last_seen,
            });
        }
        Ok(list)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParticipantInfo {
    pub key: String,
    pub display_name: Option<String>,
    pub meeting_count: usize,
    pub utterance_count: usize,
    pub sample: String,
    pub last_seen: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Vec<u8> {
        vec![7u8; 32]
    }

    fn utt(id: i64, text: &str, source: Source) -> Utterance {
        Utterance {
            id,
            speaker_id: if source == Source::Mic {
                "user".into()
            } else {
                "spk_0".into()
            },
            speaker_name: None,
            start_ms: id as u32 * 1000,
            end_ms: id as u32 * 1000 + 500,
            text: text.into(),
            source,
            low_confidence: false,
        }
    }

    #[test]
    fn cipher_roundtrips_and_rejects_wrong_key() {
        let c = Cipher::new(&key()).unwrap();
        let ct = c.encrypt("会议纪要正文").unwrap();
        assert_ne!(ct, "会议纪要正文", "密文不能等于明文");
        assert_eq!(c.decrypt(&ct).unwrap(), "会议纪要正文");

        let other = Cipher::new(&vec![9u8; 32]).unwrap();
        assert!(other.decrypt(&ct).is_err(), "换密钥必须解不开");
    }

    #[test]
    fn cipher_uses_fresh_nonce_each_time() {
        let c = Cipher::new(&key()).unwrap();
        assert_ne!(
            c.encrypt("同样的明文").unwrap(),
            c.encrypt("同样的明文").unwrap(),
            "相同明文两次加密应产生不同密文"
        );
    }

    #[test]
    fn cipher_rejects_bad_key_length() {
        assert!(Cipher::new(&[1, 2, 3]).is_err());
    }

    #[test]
    fn meeting_lifecycle() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("季度评审", "2026-08-27T10:00:00").unwrap();
        assert_eq!(s.get_meeting(id).unwrap().unwrap().status, "recording");

        s.finish_meeting(id, "2026-08-27T11:00:00", 3_600_000).unwrap();
        let m = s.get_meeting(id).unwrap().unwrap();
        assert_eq!(m.status, "recorded");
        assert_eq!(m.duration_ms, 3_600_000);
        assert_eq!(s.list_meetings().unwrap().len(), 1);
    }

    #[test]
    fn utterances_roundtrip_encrypted() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        let utts = vec![
            utt(1, "第一句", Source::System),
            utt(2, "第二句", Source::Mic),
        ];
        s.replace_utterances(id, &utts).unwrap();

        let back = s.load_utterances(id).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].text, "第一句");
        assert_eq!(back[1].source, Source::Mic);
    }

    #[test]
    fn utterance_text_is_not_stored_in_plaintext() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        s.replace_utterances(id, &[utt(1, "机密内容", Source::System)])
            .unwrap();

        let raw: String = s
            .conn
            .query_row("SELECT text FROM utterances LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.contains("机密内容"), "正文必须以密文落库");
    }

    #[test]
    fn replace_utterances_is_idempotent() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        s.replace_utterances(id, &[utt(1, "a", Source::System)]).unwrap();
        s.replace_utterances(id, &[utt(1, "b", Source::System)]).unwrap();
        let back = s.load_utterances(id).unwrap();
        assert_eq!(back.len(), 1, "重新转写不应留下旧记录");
        assert_eq!(back[0].text, "b");
    }

    #[test]
    fn naming_speakers_backfills_into_transcript() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        s.replace_utterances(id, &[utt(1, "你好", Source::System)]).unwrap();

        assert_eq!(s.distinct_speakers(id).unwrap(), vec!["spk_0"]);
        s.name_speaker(id, "spk_0", "张三").unwrap();

        let back = s.load_utterances(id).unwrap();
        assert_eq!(back[0].speaker_name.as_deref(), Some("张三"));
        assert_eq!(back[0].display_speaker(), "张三");
    }

    #[test]
    fn renaming_speaker_overwrites() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        s.name_speaker(id, "spk_0", "张三").unwrap();
        s.name_speaker(id, "spk_0", "李四").unwrap();
        assert_eq!(
            s.load_speaker_names(id).unwrap().get("spk_0").unwrap(),
            "李四"
        );
    }

    #[test]
    fn notes_and_summaries_roundtrip() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();

        assert_eq!(s.load_note(id).unwrap(), "");
        s.save_note(id, "速记：讨论预算").unwrap();
        s.save_note(id, "速记：讨论预算与排期").unwrap();
        assert_eq!(s.load_note(id).unwrap(), "速记：讨论预算与排期");

        assert!(s.latest_summary(id).unwrap().is_none());
        s.save_summary(id, "qwen", "# 纪要 v1").unwrap();
        s.save_summary(id, "qwen", "# 纪要 v2").unwrap();
        assert_eq!(s.latest_summary(id).unwrap().unwrap(), "# 纪要 v2");
    }

    #[test]
    fn egress_log_records_metadata_only() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        let rec = EgressRecord {
            endpoint: "http://localhost:11434/v1".into(),
            model: "qwen".into(),
            purpose: "summarize".into(),
            chars_sent: 1234,
            redacted: true,
            policy: "local_only".into(),
        };
        s.log_egress(Some(id), &rec).unwrap();
        assert_eq!(s.egress_count().unwrap(), 1);
    }

    #[test]
    fn meetings_paginate_newest_first() {
        let s = Store::open_in_memory(&key()).unwrap();
        for i in 0..25 {
            s.create_meeting(&format!("会议{i}"), "t").unwrap();
        }
        assert_eq!(s.count_meetings().unwrap(), 25);

        let p1 = s.list_meetings_page(0, 10).unwrap();
        assert_eq!(p1.len(), 10);
        assert_eq!(p1[0].title, "会议24", "最新的排最前");

        let p3 = s.list_meetings_page(20, 10).unwrap();
        assert_eq!(p3.len(), 5, "最后一页只剩 5 条");
        assert!(s.list_meetings_page(25, 10).unwrap().is_empty());
    }

    #[test]
    fn keywords_and_playback_roundtrip() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();

        assert!(s.get_keywords(id).unwrap().is_empty());
        assert!(s.get_playback_path(id).unwrap().is_none());

        s.set_keywords(id, &["预算".into(), "排期".into()]).unwrap();
        s.set_playback_path(id, "C:/audio/playback.wav").unwrap();

        assert_eq!(s.get_keywords(id).unwrap(), vec!["预算", "排期"]);
        assert_eq!(
            s.get_playback_path(id).unwrap().as_deref(),
            Some("C:/audio/playback.wav")
        );

        // 两个写入走的是同一行的不同列，不能互相覆盖
        s.set_keywords(id, &["新词".into()]).unwrap();
        assert_eq!(
            s.get_playback_path(id).unwrap().as_deref(),
            Some("C:/audio/playback.wav"),
            "更新关键词不应清掉回放路径"
        );
    }

    #[test]
    fn deleting_meeting_cascades() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id = s.create_meeting("m", "t").unwrap();
        s.replace_utterances(id, &[utt(1, "x", Source::System)]).unwrap();
        s.delete_meeting(id).unwrap();
        assert!(s.load_utterances(id).unwrap().is_empty());
    }

    #[test]
    fn archive_meeting_soft_deletes_from_main_list() {
        let s = Store::open_in_memory(&key()).unwrap();
        let id1 = s.create_meeting("m1", "t1").unwrap();
        let id2 = s.create_meeting("m2", "t2").unwrap();

        assert_eq!(s.count_meetings().unwrap(), 2);
        assert_eq!(s.count_archived_meetings().unwrap(), 0);
        assert!(s.list_archived_meetings().unwrap().is_empty());

        // 归档 m1
        s.archive_meeting(id1, true).unwrap();
        assert_eq!(s.count_meetings().unwrap(), 1);
        assert_eq!(s.count_archived_meetings().unwrap(), 1);

        let active = s.list_meetings().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, id2);

        let archived = s.list_archived_meetings().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, id1);
        assert!(archived[0].archived_at.is_some());

        // 恢复 m1
        s.archive_meeting(id1, false).unwrap();
        assert_eq!(s.count_meetings().unwrap(), 2);
        assert_eq!(s.count_archived_meetings().unwrap(), 0);
        assert!(s.list_archived_meetings().unwrap().is_empty());
    }
}
