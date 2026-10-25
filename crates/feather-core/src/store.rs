//! Local cache (SQLite, WAL). Everything here is re-downloadable: if the file is corrupt or from
//! an incompatible version it is moved aside and rebuilt instead of failing startup.
//!
//! Messages are stored as compact JSON blobs keyed by snowflake with a (channel_id, id) index,
//! so history pages are single index range scans. Text search uses a contentless FTS5 index.

use crate::model::{Channel, GuildInfo, Id, Message};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: &str = "2";

pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
    /// True if a corrupt/incompatible cache was discarded while opening.
    pub recovered: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadState {
    pub last_read: Id,
    pub mentions: u32,
}

pub fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
}

fn i(id: Id) -> i64 {
    id.0 as i64
}

impl Store {
    pub fn open(path: &Path) -> Store {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match Self::try_open(path) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("cache database unusable ({e}); rebuilding");
                Self::discard(path);
                match Self::try_open(path) {
                    Ok(mut s) => {
                        s.recovered = true;
                        s
                    }
                    Err(e) => {
                        log::error!("cannot create cache database ({e}); using in-memory cache");
                        let mut s = Self::open_in_memory();
                        s.recovered = true;
                        s
                    }
                }
            }
        }
    }

    fn discard(path: &Path) {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let aside = path.with_extension(format!("corrupt-{ts}"));
        let _ = std::fs::rename(path, &aside);
        for ext in ["db-wal", "db-shm"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
        // Keep at most one old copy for diagnosis.
        if let Some(dir) = path.parent() {
            if let Ok(rd) = std::fs::read_dir(dir) {
                let mut olds: Vec<PathBuf> =
                    rd.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".corrupt-")).collect();
                olds.sort();
                olds.pop();
                for o in olds {
                    let _ = std::fs::remove_file(o);
                }
            }
        }
    }

    fn try_open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        let mut s = Store { conn, path: Some(path.to_path_buf()), recovered: false };
        s.init()?;
        Ok(s)
    }

    pub fn open_in_memory() -> Store {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let mut s = Store { conn, path: None, recovered: false };
        s.init().expect("schema");
        s
    }

    fn init(&mut self) -> rusqlite::Result<()> {
        let c = &self.conn;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "NORMAL")?;
        c.pragma_update(None, "temp_store", "MEMORY")?;
        c.pragma_update(None, "cache_size", -2048)?; // 2 MiB page cache
        c.pragma_update(None, "foreign_keys", false)?;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS guilds(id INTEGER PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS dms(id INTEGER PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS messages(channel_id INTEGER NOT NULL, id INTEGER NOT NULL, json TEXT NOT NULL, PRIMARY KEY(channel_id, id)) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS messages_id ON messages(id);
             CREATE TABLE IF NOT EXISTS read_state(channel_id INTEGER PRIMARY KEY, last_read INTEGER NOT NULL, mentions INTEGER NOT NULL DEFAULT 0);
             CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(content, content='', contentless_delete=1, tokenize='unicode61');",
        )?;
        match self.meta_get_raw("schema")? {
            None => {
                self.meta_set("schema", SCHEMA_VERSION)?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
                    Some(format!("cache schema {v} is not supported")),
                ))
            }
        }
        Ok(())
    }

    fn meta_get_raw(&self, k: &str) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key=?", [k], |r| r.get(0)).optional()
    }

    pub fn meta_get(&self, k: &str) -> Option<String> {
        self.meta_get_raw(k).ok().flatten()
    }

    pub fn meta_set(&self, k: &str, v: &str) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [k, v])?;
        Ok(())
    }

    pub fn save_guild(&self, g: &GuildInfo) -> rusqlite::Result<()> {
        let j = serde_json::to_string(g).expect("guild serialises");
        self.conn.execute("INSERT OR REPLACE INTO guilds(id,json) VALUES(?,?)", params![i(g.id), j])?;
        Ok(())
    }

    pub fn remove_guild(&self, id: Id) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM guilds WHERE id=?", [i(id)])?;
        Ok(())
    }

    pub fn load_guilds(&self) -> rusqlite::Result<Vec<GuildInfo>> {
        let mut st = self.conn.prepare("SELECT json FROM guilds")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    pub fn save_dm(&self, c: &Channel) -> rusqlite::Result<()> {
        let j = serde_json::to_string(c).expect("channel serialises");
        self.conn.execute("INSERT OR REPLACE INTO dms(id,json) VALUES(?,?)", params![i(c.id), j])?;
        Ok(())
    }

    pub fn load_dms(&self) -> rusqlite::Result<Vec<Channel>> {
        let mut st = self.conn.prepare("SELECT json FROM dms")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    /// Upserts messages in one transaction and keeps the search index in step.
    pub fn insert_messages(&mut self, msgs: &[Message]) -> rusqlite::Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached("INSERT OR REPLACE INTO messages(id,channel_id,json) VALUES(?,?,?)")?;
            let mut del = tx.prepare_cached("DELETE FROM fts WHERE rowid=?")?;
            let mut fts = tx.prepare_cached("INSERT INTO fts(rowid,content) VALUES(?,?)")?;
            let mut cur = tx.prepare_cached("SELECT json FROM messages WHERE id=?")?;
            for m in msgs {
                let j = serde_json::to_string(m).expect("message serialises");
                // Re-fetched history is mostly unchanged; skipping identical rows avoids rewriting
                // table, index and search pages (the dominant disk write when switching channels).
                if cur.query_row([i(m.id)], |r| r.get::<_, String>(0)).optional()?.is_some_and(|old| old == j) {
                    continue;
                }
                ins.execute(params![i(m.id), i(m.channel_id), j])?;
                del.execute([i(m.id)])?;
                if !m.content.is_empty() {
                    fts.execute(params![i(m.id), m.content])?;
                }
            }
        }
        tx.commit()
    }

    pub fn get_message(&self, id: Id) -> rusqlite::Result<Option<Message>> {
        let j: Option<String> = self.conn.query_row("SELECT json FROM messages WHERE id=?", [i(id)], |r| r.get(0)).optional()?;
        Ok(j.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn delete_messages(&mut self, ids: &[Id]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut d = tx.prepare_cached("DELETE FROM messages WHERE id=?")?;
            let mut f = tx.prepare_cached("DELETE FROM fts WHERE rowid=?")?;
            for id in ids {
                d.execute([i(*id)])?;
                f.execute([i(*id)])?;
            }
        }
        tx.commit()
    }

    fn query_msgs(&self, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<Vec<Message>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st.query_map(p, |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    /// Up to `limit` messages older than `before` (or the newest), returned oldest first.
    pub fn messages_before(&self, channel: Id, before: Option<Id>, limit: u32) -> rusqlite::Result<Vec<Message>> {
        let b = before.map(i).unwrap_or(i64::MAX);
        let mut v = self.query_msgs(
            "SELECT json FROM messages WHERE channel_id=? AND id<? ORDER BY id DESC LIMIT ?",
            params![i(channel), b, limit],
        )?;
        v.reverse();
        Ok(v)
    }

    /// Up to `limit` messages newer than `after`, oldest first.
    pub fn messages_after(&self, channel: Id, after: Id, limit: u32) -> rusqlite::Result<Vec<Message>> {
        self.query_msgs(
            "SELECT json FROM messages WHERE channel_id=? AND id>? ORDER BY id ASC LIMIT ?",
            params![i(channel), i(after), limit],
        )
    }

    pub fn newest_id(&self, channel: Id) -> rusqlite::Result<Option<Id>> {
        self.conn
            .query_row("SELECT MAX(id) FROM messages WHERE channel_id=?", [i(channel)], |r| r.get::<_, Option<i64>>(0))
            .map(|o| o.map(|v| Id(v as u64)))
    }

    pub fn read_states(&self) -> rusqlite::Result<HashMap<Id, ReadState>> {
        let mut st = self.conn.prepare("SELECT channel_id,last_read,mentions FROM read_state")?;
        let rows = st.query_map([], |r| {
            Ok((Id(r.get::<_, i64>(0)? as u64), ReadState { last_read: Id(r.get::<_, i64>(1)? as u64), mentions: r.get(2)? }))
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn set_read_states(&mut self, states: &[(Id, ReadState)]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO read_state(channel_id,last_read,mentions) VALUES(?,?,?)
                 ON CONFLICT(channel_id) DO UPDATE SET last_read=excluded.last_read, mentions=excluded.mentions",
            )?;
            for (c, s) in states {
                st.execute(params![i(*c), i(s.last_read), s.mentions])?;
            }
        }
        tx.commit()
    }

    /// Deletes all but the newest `keep` messages of a channel. Returns rows removed.
    pub fn prune_channel(&mut self, channel: Id, keep: u32) -> rusqlite::Result<usize> {
        let cutoff: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM messages WHERE channel_id=? ORDER BY id DESC LIMIT 1 OFFSET ?",
                params![i(channel), keep],
                |r| r.get(0),
            )
            .optional()?;
        let Some(cutoff) = cutoff else { return Ok(0) };
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM fts WHERE rowid IN (SELECT id FROM messages WHERE channel_id=? AND id<=?)",
            params![i(channel), cutoff],
        )?;
        let n = tx.execute("DELETE FROM messages WHERE channel_id=? AND id<=?", params![i(channel), cutoff])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn channel_message_count(&self, channel: Id) -> rusqlite::Result<u32> {
        self.conn.query_row("SELECT COUNT(*) FROM messages WHERE channel_id=?", [i(channel)], |r| r.get(0))
    }

    /// Full-text search over cached messages. User input is turned into quoted terms, so FTS
    /// query syntax in the input is treated literally.
    pub fn search(&self, query: &str, channel: Option<Id>, limit: u32) -> rusqlite::Result<Vec<Message>> {
        let terms: Vec<String> = query
            .split_whitespace()
            .take(16)
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut q = terms.join(" ");
        q.push('*');
        match channel {
            Some(c) => self.query_msgs(
                "SELECT m.json FROM fts JOIN messages m ON m.id=fts.rowid WHERE fts MATCH ? AND m.channel_id=? ORDER BY m.id DESC LIMIT ?",
                params![q, i(c), limit],
            ),
            None => self.query_msgs(
                "SELECT m.json FROM fts JOIN messages m ON m.id=fts.rowid WHERE fts MATCH ? ORDER BY m.id DESC LIMIT ?",
                params![q, limit],
            ),
        }
    }

    /// Removes everything (used on logout so another bot's data is never shown).
    pub fn clear_all(&mut self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM guilds; DELETE FROM dms; DELETE FROM messages; DELETE FROM read_state; DELETE FROM fts;
             DELETE FROM meta WHERE key<>'schema';",
        )?;
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
        Ok(())
    }

    pub fn total_messages(&self) -> u64 {
        self.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64
    }

    pub fn file_size(&self) -> u64 {
        let Some(p) = &self.path else { return 0 };
        ["", "-wal"]
            .iter()
            .filter_map(|suf| std::fs::metadata(format!("{}{suf}", p.display())).ok())
            .map(|m| m.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::User;

    fn msg(id: u64, ch: u64, content: &str) -> Message {
        Message {
            id: Id(id),
            channel_id: Id(ch),
            content: content.into(),
            author: User { id: Id(1), username: "u".into(), ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn history_paging_and_upsert() {
        let mut s = Store::open_in_memory();
        let v: Vec<Message> = (1..=100).map(|n| msg(n, 7, &format!("m{n}"))).collect();
        s.insert_messages(&v).unwrap();
        s.insert_messages(&[msg(5, 7, "edited")]).unwrap();
        let newest = s.messages_before(Id(7), None, 10).unwrap();
        assert_eq!(newest.first().unwrap().id, Id(91));
        assert_eq!(newest.last().unwrap().id, Id(100));
        let older = s.messages_before(Id(7), Some(Id(91)), 10).unwrap();
        assert_eq!(older.last().unwrap().id, Id(90));
        assert_eq!(s.get_message(Id(5)).unwrap().unwrap().content, "edited");
        assert_eq!(s.newest_id(Id(7)).unwrap(), Some(Id(100)));
        assert_eq!(s.messages_after(Id(7), Id(98), 10).unwrap().len(), 2);
        assert_eq!(s.newest_id(Id(8)).unwrap(), None);
    }

    #[test]
    fn prune_keeps_newest_and_index_consistent() {
        let mut s = Store::open_in_memory();
        let v: Vec<Message> = (1..=50).map(|n| msg(n, 7, "needle")).collect();
        s.insert_messages(&v).unwrap();
        assert_eq!(s.prune_channel(Id(7), 20).unwrap(), 30);
        assert_eq!(s.channel_message_count(Id(7)).unwrap(), 20);
        assert_eq!(s.search("needle", None, 100).unwrap().len(), 20);
    }

    #[test]
    fn search_is_literal_and_tracks_edits() {
        let mut s = Store::open_in_memory();
        s.insert_messages(&[msg(1, 7, "hello world"), msg(2, 8, "goodbye world"), msg(3, 7, "NEAR(a b) \"quote")]).unwrap();
        assert_eq!(s.search("world", None, 10).unwrap().len(), 2);
        assert_eq!(s.search("world", Some(Id(7)), 10).unwrap().len(), 1);
        assert_eq!(s.search("hel", None, 10).unwrap().len(), 1, "prefix match");
        assert!(s.search("NEAR(a b) \"", None, 10).is_ok());
        s.insert_messages(&[msg(1, 7, "changed text")]).unwrap();
        assert_eq!(s.search("hello", None, 10).unwrap().len(), 0);
        s.delete_messages(&[Id(2)]).unwrap();
        assert_eq!(s.search("world", None, 10).unwrap().len(), 0);
    }

    #[test]
    fn read_states_roundtrip() {
        let mut s = Store::open_in_memory();
        s.set_read_states(&[(Id(1), ReadState { last_read: Id(10), mentions: 2 })]).unwrap();
        s.set_read_states(&[(Id(1), ReadState { last_read: Id(11), mentions: 0 })]).unwrap();
        assert_eq!(s.read_states().unwrap()[&Id(1)], ReadState { last_read: Id(11), mentions: 0 });
    }

    #[test]
    fn corrupt_file_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache.db");
        std::fs::write(&p, b"this is definitely not a sqlite database, just garbage bytes......").unwrap();
        let mut s = Store::open(&p);
        assert!(s.recovered);
        s.insert_messages(&[msg(1, 1, "ok")]).unwrap();
        drop(s);
        let s = Store::open(&p);
        assert!(!s.recovered);
        assert_eq!(s.total_messages(), 1);
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| e.path().to_string_lossy().contains("corrupt-")));
    }

    #[test]
    fn future_schema_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache.db");
        {
            let s = Store::open(&p);
            s.meta_set("schema", "999").unwrap();
        }
        let s = Store::open(&p);
        assert!(s.recovered);
    }
}
