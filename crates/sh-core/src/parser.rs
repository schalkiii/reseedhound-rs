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

    let results: Vec<Option<TorrentInfo>> = files.par_iter().map(|p| parse_file(p)).collect();
    Ok(results.into_iter().flatten().collect())
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
