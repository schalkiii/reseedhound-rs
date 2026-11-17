//! SQLite 缓存（对齐 Python src/storage/cache.py）。
//!
//! 三表 schema 与 Python 版完全兼容（老库直接可用）：
//! - pieces_cache / reseed_history / dead_torrents
//! - reseed_history 唯一索引首次升级先去重（老库兼容）
//!
//! Python 版每操作开新连接（aiosqlite.connect per call）；Rust 版持有单个
//! Mutex<Connection>——语义等价（SQLite 串行写），且免每次连接开销。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

pub struct TorrentCache {
    conn: Mutex<Connection>,
    _db_dir: PathBuf,
}

impl TorrentCache {
    pub fn open(db_path: &str) -> anyhow::Result<Self> {
        let path = Path::new(db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let cache = Self {
            conn: Mutex::new(conn),
            _db_dir: path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        };
        cache.init()?;
        Ok(cache)
    }

    fn init(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pieces_cache (
                pieces_hash TEXT PRIMARY KEY,
                info_hash TEXT NOT NULL,
                file_name TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS reseed_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pieces_hash TEXT NOT NULL,
                site_name TEXT NOT NULL,
                torrent_id INTEGER NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_pieces_hash ON pieces_cache(pieces_hash);
            CREATE TABLE IF NOT EXISTS dead_torrents (
                site_name TEXT NOT NULL,
                torrent_id INTEGER NOT NULL,
                first_dead_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                last_dead_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (site_name, torrent_id)
            );
            CREATE INDEX IF NOT EXISTS idx_dead_first ON dead_torrents(first_dead_at);",
        )?;
        // 唯一索引首次升级：先去重（对齐 Python 首次升级逻辑）
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='uq_reseed_history'",
                [],
                |row| row.get::<_, i64>(0),
            )? > 0;
        if !exists {
            conn.execute_batch(
                "DELETE FROM reseed_history WHERE id NOT IN (
                    SELECT MIN(id) FROM reseed_history
                    GROUP BY pieces_hash, site_name, torrent_id
                );
                CREATE UNIQUE INDEX IF NOT EXISTS uq_reseed_history
                ON reseed_history(pieces_hash, site_name, torrent_id);",
            )?;
        }
        Ok(())
    }

    /// 返回 pieces_hash 不在缓存中的条目索引集合语义：
    /// 直接返回未缓存的 pieces_hash 列表（500 一批，对齐 Python batch 语义）。
    pub fn filter_uncached(&self, hashes: &[String]) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut uncached = Vec::new();
        for batch in hashes.chunks(500) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT pieces_hash FROM pieces_cache WHERE pieces_hash IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let cached: std::collections::HashSet<String> = stmt
                .query_map(rusqlite::params_from_iter(batch.iter()), |row| {
                    row.get::<_, String>(0)
                })?
                .filter_map(|r| r.ok())
                .collect();
            uncached.extend(
                batch
                    .iter()
                    .filter(|h| !cached.contains(*h))
                    .cloned(),
            );
        }
        Ok(uncached)
    }

    pub fn add_batch(
        &self,
        entries: &[(String, String, String)], // (pieces_hash, info_hash, file_name)
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("INSERT OR IGNORE INTO pieces_cache (pieces_hash, info_hash, file_name) VALUES (?, ?, ?)")?;
            for (ph, ih, name) in entries {
                stmt.execute(rusqlite::params![ph, ih, name])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_reseed_record(
        &self,
        pieces_hash: &str,
        site_name: &str,
        torrent_id: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO reseed_history (pieces_hash, site_name, torrent_id) VALUES (?, ?, ?)",
            rusqlite::params![pieces_hash, site_name, torrent_id],
        )?;
        Ok(())
    }

    /// 返回 {site_name: {pieces_hash}}（只含传入列表中命中的）
    pub fn get_reseeded_hashes_by_site(
        &self,
        hashes: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, std::collections::HashSet<String>>> {
        let mut result: std::collections::HashMap<String, std::collections::HashSet<String>> =
            Default::default();
        if hashes.is_empty() {
            return Ok(result);
        }
        let conn = self.conn.lock().unwrap();
        for batch in hashes.chunks(500) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT DISTINCT pieces_hash, site_name FROM reseed_history WHERE pieces_hash IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(batch.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (ph, site) = row?;
                result.entry(site).or_default().insert(ph);
            }
        }
        Ok(result)
    }

    pub fn get_stats(&self) -> anyhow::Result<(i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let cached: i64 =
            conn.query_row("SELECT COUNT(*) FROM pieces_cache", [], |r| r.get(0))?;
        let reseeded: i64 =
            conn.query_row("SELECT COUNT(*) FROM reseed_history", [], |r| r.get(0))?;
        Ok((cached, reseeded))
    }

    /// 持续死亡 ≥ min_days 天的 (site, torrent_id)
    pub fn load_persistent_dead(
        &self,
        min_days: f64,
    ) -> anyhow::Result<std::collections::HashSet<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT site_name, torrent_id FROM dead_torrents \
             WHERE julianday('now') - julianday(first_dead_at) >= ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![min_days], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    pub fn record_dead(&self, site_name: &str, torrent_id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO dead_torrents (site_name, torrent_id, first_dead_at, last_dead_at) \
             VALUES (?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
             ON CONFLICT(site_name, torrent_id) DO UPDATE SET last_dead_at = CURRENT_TIMESTAMP",
            rusqlite::params![site_name, torrent_id],
        )?;
        Ok(())
    }

    pub fn clear_dead_if_alive(&self, site_name: &str, torrent_id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM dead_torrents WHERE site_name = ? AND torrent_id = ?",
            rusqlite::params![site_name, torrent_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("sh_cache_test_{name}.db"));
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn cache_roundtrip() {
        let db = tmp_db("roundtrip");
        let cache = TorrentCache::open(&db).unwrap();

        let uncached = cache
            .filter_uncached(&["a".into(), "b".into(), "c".into()])
            .unwrap();
        assert_eq!(uncached.len(), 3);

        cache
            .add_batch(&[
                ("a".into(), "ia".into(), "fa".into()),
                ("b".into(), "ib".into(), "fb".into()),
            ])
            .unwrap();

        let uncached = cache
            .filter_uncached(&["a".into(), "b".into(), "c".into()])
            .unwrap();
        assert_eq!(uncached, vec!["c".to_string()]);

        cache.add_reseed_record("a", "site1", 42).unwrap();
        // 幂等：唯一索引去重
        cache.add_reseed_record("a", "site1", 42).unwrap();
        let by_site = cache.get_reseeded_hashes_by_site(&["a".into()]).unwrap();
        assert_eq!(by_site["site1"].len(), 1);

        let (cached, reseeded) = cache.get_stats().unwrap();
        assert_eq!((cached, reseeded), (2, 1));
    }

    #[test]
    fn dead_torrent_upsert_and_clear() {
        let db = tmp_db("dead");
        let cache = TorrentCache::open(&db).unwrap();
        cache.record_dead("s", 1).unwrap();
        cache.record_dead("s", 1).unwrap(); // upsert 不重复
        // 刚写入的记录不满 15 天，不入 persistent 集合
        let persistent = cache.load_persistent_dead(15.0).unwrap();
        assert!(persistent.is_empty());
        cache.clear_dead_if_alive("s", 1).unwrap();
        let (cached, reseeded) = cache.get_stats().unwrap();
        assert_eq!((cached, reseeded), (0, 0));
    }

    #[test]
    fn reopen_keeps_data() {
        let db = tmp_db("reopen");
        {
            let cache = TorrentCache::open(&db).unwrap();
            cache
                .add_batch(&[("x".into(), "ix".into(), "fx".into())])
                .unwrap();
        }
        let cache = TorrentCache::open(&db).unwrap();
        let uncached = cache.filter_uncached(&["x".into()]).unwrap();
        assert!(uncached.is_empty());
    }
}
