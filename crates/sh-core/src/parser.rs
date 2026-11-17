//! torrent 解析与目录扫描。
//!
//! 对齐 Python src/engine/parser.py：
//! - `parse_bytes` 容错语义（无 info / 无 pieces → None）
//! - info_hash 用 info 字典的**原始字节区间**直接 SHA1（规避重编码差异）
//! - pieces_hash = SHA1(pieces 原始字节)
//! - scan_directory 用 rayon 并行（替代 Python 的 100 并发线程池）

use std::path::{Path, PathBuf};

use crate::bencode::{bdecode, BencodeRef};
use crate::hash::sha1_hex;
use rayon::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentInfo {
    pub info_hash: String,
    pub pieces_hash: String,
    pub file_name: String,
    pub file_path: String,
    pub torrent_name: Option<String>,
    pub total_size: Option<i64>,
    pub announce: Option<String>,
    pub announce_list: Option<Vec<String>>,
}

/// 解析一段 torrent 字节。返回 None 与 Python 版语义一致（解析失败静默跳过）。
pub fn parse_bytes(data: &[u8], file_name: &str, file_path: &str) -> Option<TorrentInfo> {
    let torrent = bdecode(data).ok()?;
    let BencodeRef::Dict(root, _) = &torrent else {
        return None;
    };
    let info = root.get(b"info".as_slice())?;
    let BencodeRef::Dict(info_map, info_range) = info else {
        return None;
    };
    let pieces = match info_map.get(b"pieces".as_slice())? {
        BencodeRef::Bytes(b) => *b,
        _ => return None,
    };

    let info_hash = sha1_hex(&data[info_range.clone()]);
    let pieces_hash = sha1_hex(pieces);

    let torrent_name = match info_map.get(b"name".as_slice()) {
        Some(BencodeRef::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    };

    let total_size = match info_map.get(b"length".as_slice()) {
        Some(BencodeRef::Int(n)) => Some(*n),
        _ => match info_map.get(b"files".as_slice()) {
            Some(BencodeRef::List(files)) => {
                let mut total = 0i64;
                let mut found = false;
                for f in files {
                    if let BencodeRef::Dict(fm, _) = f {
                        if let Some(BencodeRef::Int(n)) = fm.get(b"length".as_slice()) {
                            total += n;
                            found = true;
                        }
                    }
                }
                if found {
                    Some(total)
                } else {
                    None
                }
            }
            _ => None,
        },
    };

    let announce = match root.get(b"announce".as_slice()) {
        Some(BencodeRef::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    };

    let announce_list = match root.get(b"announce-list".as_slice()) {
        Some(BencodeRef::List(tiers)) => {
            let mut urls = Vec::new();
            for tier in tiers {
                if let BencodeRef::List(items) = tier {
                    for url in items {
                        if let BencodeRef::Bytes(b) = url {
                            urls.push(String::from_utf8_lossy(b).into_owned());
                        }
                    }
                }
            }
            if urls.is_empty() {
                None
            } else {
                Some(urls)
            }
        }
        _ => None,
    };

    Some(TorrentInfo {
        info_hash,
        pieces_hash,
        file_name: file_name.to_string(),
        file_path: file_path.to_string(),
        torrent_name,
        total_size,
        announce,
        announce_list,
    })
}

/// 解析单个 .torrent 文件。
pub fn parse_file(path: &Path) -> Option<TorrentInfo> {
    let data = std::fs::read(path).ok()?;
    parse_bytes(&data, &path.file_name()?.to_string_lossy(), &path.to_string_lossy())
}

/// 扫描目录当前层的 *.torrent（不递归），rayon 并行解析。
/// 目录不存在返回 Err（对齐 Python FileNotFoundError）。
/// 扫描结果（含文件级解析缓存统计）。
pub struct ScanResult {
    pub torrents: Vec<TorrentInfo>,
    /// path+mtime+size 命中缓存、跳过解析的文件数
    pub cache_hits: usize,
    /// 实际读盘解析的文件数
    pub parsed: usize,
}

/// 列举目录下的 .torrent 文件及其身份（mtime 秒 + 字节数）。
///
/// 目录列举用 `file_type()`（Windows 上来自 FindFirstFile，零额外系统调用）；
/// metadata 并行获取——4 万级目录顺序 stat 实测要 30s+，并行后秒级。
fn list_torrent_files(dir_path: &Path) -> anyhow::Result<Vec<(PathBuf, i64, i64)>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().map(|ext| ext == "torrent").unwrap_or(false) {
            paths.push(path);
        }
    }
    let files: Vec<(PathBuf, i64, i64)> = paths
        .par_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            let secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some((path.clone(), secs, meta.len() as i64))
        })
        .collect();
    Ok(files)
}

pub fn scan_directory(dir: &str) -> anyhow::Result<Vec<TorrentInfo>> {
    let dir_path = Path::new(dir);
    if !dir_path.exists() {
        anyhow::bail!("种子目录不存在: {dir}");
    }
    let files: Vec<PathBuf> = std::fs::read_dir(dir_path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file() && p.extension().map(|ext| ext == "torrent").unwrap_or(false)
        })
        .collect();
    tracing::info!("发现 {} 个种子文件", files.len());

    // 解析进度：4 万级目录耗时可达数十秒，每 5000 个报一行
    let total = files.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Option<TorrentInfo>> = files
        .par_iter()
        .map(|p| {
            let info = parse_file(p);
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_multiple_of(5000) || n == total {
                tracing::info!("[阶段1] 解析进度 {n}/{total}");
            }
            info
        })
        .collect();
    Ok(results.into_iter().flatten().collect())
}

/// 带文件级解析缓存的扫描（定时轮次的常态路径）。
///
/// 以 (path, mtime, size) 为键命中缓存时直接重建 `TorrentInfo`，
/// 只对新增/变更文件做读盘 + bencode 解析；`pieces_hash` 只能从内容算出，
/// 因此首轮仍需全量解析，收益在重复运行。
pub fn scan_directory_with_cache(
    dir: &str,
    cache: &crate::cache::TorrentCache,
) -> anyhow::Result<ScanResult> {
    let dir_path = Path::new(dir);
    if !dir_path.exists() {
        anyhow::bail!("种子目录不存在: {dir}");
    }
    let files = list_torrent_files(dir_path)?;
    tracing::info!("发现 {} 个种子文件", files.len());

    let keys: Vec<(String, i64, i64)> = files
        .iter()
        .map(|(p, m, s)| (p.to_string_lossy().into_owned(), *m, *s))
        .collect();
    let mut cached = cache.lookup_file_cache(&keys)?;
    let cache_hits = cached.len();

    let misses: Vec<&(PathBuf, i64, i64)> = files
        .iter()
        .filter(|(p, _, _)| !cached.contains_key(p.to_string_lossy().as_ref()))
        .collect();
    let miss_total = misses.len();

    // 仅对未命中文件解析；进度按解析数计
    let done = std::sync::atomic::AtomicUsize::new(0);
    let parsed: Vec<Option<TorrentInfo>> = misses
        .par_iter()
        .map(|(p, _, _)| {
            let info = parse_file(p);
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_multiple_of(5000) || n == miss_total {
                tracing::info!("[阶段1] 解析进度 {n}/{miss_total}（缓存命中 {cache_hits}）");
            }
            info
        })
        .collect();

    let mut store_entries = Vec::new();
    let mut torrents = Vec::new();
    for ((path, mtime, size), info) in misses.iter().zip(parsed) {
        if let Some(info) = info {
            store_entries.push((path.to_string_lossy().into_owned(), *mtime, *size, info.clone()));
            torrents.push(info);
        }
    }
    if !store_entries.is_empty() {
        cache.store_file_cache(&store_entries)?;
    }
    torrents.extend(cached.drain().map(|(path, mut info)| {
        // 缓存行不存 file_path（键本身就是路径），重建时补齐
        info.file_path = path;
        info
    }));

    // 修剪已删除文件的陈旧行
    let keep: std::collections::HashSet<String> = keys.into_iter().map(|k| k.0).collect();
    let pruned = cache.prune_file_cache(&keep)?;
    if pruned > 0 {
        tracing::info!("文件解析缓存裁剪 {} 行（对应文件已删除）", pruned);
    }

    Ok(ScanResult {
        torrents,
        cache_hits,
        parsed: miss_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python tests/helpers.py 的 make_torrent_bytes 等价构造：
    /// 直接拼 bencode 原始字节（不搭 BencodeRef 树，规避借用问题）。
    /// 生成结构与 Python 完全一致：announce / info{length,name,piece length,pieces}。
    fn make_torrent_bytes(name: &str, pieces: Option<&[u8]>) -> Vec<u8> {
        let empty_pieces = [0u8; 20];
        let pieces = pieces.unwrap_or(&empty_pieces);
        let mut out = Vec::new();
        out.extend_from_slice(b"d8:announce27:http://example.com/announce4:infod");
        // info 内字段按字节序：length < name < piece length < pieces
        out.extend_from_slice(b"6:lengthi1234e");
        out.extend_from_slice(format!("4:name{}:{}", name.len(), name).as_bytes());
        out.extend_from_slice(b"12:piece lengthi65536e");
        out.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        out.extend_from_slice(pieces);
        out.extend_from_slice(b"ee");
        out
    }

    /// golden 值来自 Python tests/helpers.py（外部手写）：
    /// INFO_HASH_GOLDEN / PIECES_HASH_GOLDEN
    #[test]
    fn parse_matches_python_golden() {
        let data = make_torrent_bytes("test.dat", None);
        let parsed = parse_bytes(&data, "test.dat", "").unwrap();
        assert_eq!(parsed.info_hash, "35471bc58ffcae4564c40faf40ff75dbbf97c36a");
        assert_eq!(parsed.pieces_hash, "6768033e216468247bd031a0a2d9876d79818f8f");
        assert_eq!(parsed.total_size, Some(1234));
        assert_eq!(parsed.torrent_name.as_deref(), Some("test.dat"));
        assert_eq!(
            parsed.announce.as_deref(),
            Some("http://example.com/announce")
        );
    }

    #[test]
    fn parse_rejects_missing_info_or_pieces() {
        assert!(parse_bytes(b"i42e", "", "").is_none());
        // dict 无 info
        assert!(parse_bytes(b"d3:foo3:bare", "", "").is_none());
        // info 无 pieces
        let torrent = b"d4:infod6:lengthi1e4:name1:xee".to_vec();
        assert!(parse_bytes(&torrent, "", "").is_none());
    }

    #[test]
    fn parse_files_sum_total_size() {
        // 多文件种子：info.files 列表求和（files 列表在两个 file dict 后需 'e' 关闭）
        let mut torrent: Vec<u8> =
            b"d4:infod5:filesld6:lengthi100e4:path1:aed6:lengthi23e4:path1:bee"
                .to_vec();
        torrent.extend_from_slice(b"4:name3:dir6:pieces20:");
        torrent.extend_from_slice(&[0u8; 20]);
        torrent.extend_from_slice(b"ee");
        let parsed = parse_bytes(&torrent, "", "").unwrap();
        assert_eq!(parsed.total_size, Some(123));
    }

    #[test]
    fn scan_directory_with_cache_skips_unchanged_files() {
        let dir = std::env::temp_dir().join("sh_core_filecache_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            let data = make_torrent_bytes(&format!("t{i}.dat"), None);
            std::fs::write(dir.join(format!("t{i}.torrent")), data).unwrap();
        }
        let db = dir.join("cache.db");
        let cache = crate::cache::TorrentCache::open(db.to_str().unwrap()).unwrap();

        // 首轮：全部解析，零命中
        let first = scan_directory_with_cache(dir.to_str().unwrap(), &cache).unwrap();
        assert_eq!(first.parsed, 5);
        assert_eq!(first.cache_hits, 0);
        assert_eq!(first.torrents.len(), 5);

        // 二轮：全部命中缓存，跳过解析；解析产物字段一致
        let second = scan_directory_with_cache(dir.to_str().unwrap(), &cache).unwrap();
        assert_eq!(second.parsed, 0);
        assert_eq!(second.cache_hits, 5);
        let key = |t: &TorrentInfo| {
            (
                t.info_hash.clone(),
                t.pieces_hash.clone(),
                t.file_name.clone(),
                t.torrent_name.clone(),
                t.total_size,
                t.announce.clone(),
                t.announce_list.clone(),
            )
        };
        let mut a: Vec<_> = first.torrents.iter().map(key).collect();
        let mut b: Vec<_> = second.torrents.iter().map(key).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);

        // 改一个文件（内容不同 → size 不同）→ 只解析它
        let data = make_torrent_bytes("changed-name.dat", None);
        std::fs::write(dir.join("t0.torrent"), data).unwrap();
        let third = scan_directory_with_cache(dir.to_str().unwrap(), &cache).unwrap();
        assert_eq!(third.parsed, 1);
        assert_eq!(third.cache_hits, 4);

        // 删除一个文件 → 下轮裁剪陈旧行
        std::fs::remove_file(dir.join("t4.torrent")).unwrap();
        let fourth = scan_directory_with_cache(dir.to_str().unwrap(), &cache).unwrap();
        assert_eq!(fourth.torrents.len(), 4);
        let conn_count: i64 = {
            use rusqlite::Connection;
            let conn = Connection::open(&db).unwrap();
            conn.query_row("SELECT COUNT(*) FROM file_cache", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(conn_count, 4, "陈旧行应被裁剪");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_directory_parallel() {
        let dir = std::env::temp_dir().join("sh_core_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            let data = make_torrent_bytes(&format!("t{i}.dat"), None);
            std::fs::write(dir.join(format!("t{i}.torrent")), data).unwrap();
        }
        std::fs::write(dir.join("ignored.txt"), b"not a torrent").unwrap();
        let results = scan_directory(dir.to_str().unwrap()).unwrap();
        assert_eq!(results.len(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
