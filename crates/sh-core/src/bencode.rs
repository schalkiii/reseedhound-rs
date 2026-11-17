//! bencode 编解码：零拷贝游标实现。
//!
//! 解码返回 `BencodeRef`（借用原始字节），info 字典的原始字节区间
//! 用 `Range<usize>` 记录，info_hash 直接对区间做 SHA1，
//! 避免「重编码排序差异」这一 Python 版踩过的坑（见 PYTHON-INDEX §13.1）。

use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum BencodeRef<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<BencodeRef<'a>>),
    /// 键按 bencode 规范（字节序）排序存储；`raw_range` 记录整个字典的原始字节区间
    Dict(BTreeMap<Vec<u8>, BencodeRef<'a>>, Range<usize>),
}

#[derive(Debug, thiserror::Error)]
pub enum BencodeError {
    #[error("无效 bencode 数据: 位置 {position}")]
    Invalid { position: usize },
    #[error("数据截断: 位置 {position}")]
    Truncated { position: usize },
    #[error("整数解析失败: {0}")]
    IntParse(String),
    #[error("长度前缀解析失败: 位置 {position}")]
    LengthParse { position: usize },
}

/// 从字节切片解码顶层值；要求完整消费（多余尾部字节视为错误，与 Python 版
/// 只解码顶层一次的行为一致：Python 版忽略尾部，这里同样忽略尾部以对齐）。
pub fn bdecode(data: &[u8]) -> Result<BencodeRef<'_>, BencodeError> {
    let mut cursor = 0usize;
    let value = decode_value(data, &mut cursor)?;
    Ok(value)
}

fn decode_value<'a>(
    data: &'a [u8],
    cursor: &mut usize,
) -> Result<BencodeRef<'a>, BencodeError> {
    let pos = *cursor;
    let Some(&first) = data.get(pos) else {
        return Err(BencodeError::Truncated { position: pos });
    };
    match first {
        b'i' => {
            // i<int>e
            let rest = &data[pos + 1..];
            let Some(end_rel) = rest.iter().position(|&b| b == b'e') else {
                return Err(BencodeError::Truncated { position: pos });
            };
            let text = std::str::from_utf8(&rest[..end_rel])
                .map_err(|_| BencodeError::IntParse("非 UTF-8".into()))?;
            let value: i64 = text
                .parse()
                .map_err(|e: std::num::ParseIntError| BencodeError::IntParse(e.to_string()))?;
            *cursor = pos + 1 + end_rel + 1;
            Ok(BencodeRef::Int(value))
        }
        b'l' => {
            *cursor = pos + 1;
            let mut items = Vec::new();
            while data.get(*cursor).copied() != Some(b'e') {
                if *cursor >= data.len() {
                    return Err(BencodeError::Truncated { position: *cursor });
                }
                items.push(decode_value(data, cursor)?);
            }
            *cursor += 1; // consume 'e'
            Ok(BencodeRef::List(items))
        }
        b'd' => {
            let start = pos;
            *cursor = pos + 1;
            let mut map = BTreeMap::new();
            while data.get(*cursor).copied() != Some(b'e') {
                if *cursor >= data.len() {
                    return Err(BencodeError::Truncated { position: *cursor });
                }
                let key = decode_bytes_value(data, cursor)?;
                let key = match key {
                    BencodeRef::Bytes(b) => b.to_vec(),
                    _ => return Err(BencodeError::Invalid { position: *cursor }),
                };
                let value = decode_value(data, cursor)?;
                map.insert(key, value);
            }
            *cursor += 1; // consume 'e'
            Ok(BencodeRef::Dict(map, start..*cursor))
        }
        b'0'..=b'9' => {
            let bytes = decode_bytes_value(data, cursor)?;
            match bytes {
                BencodeRef::Bytes(b) => Ok(BencodeRef::Bytes(b)),
                _ => unreachable!(),
            }
        }
        _ => Err(BencodeError::Invalid { position: pos }),
    }
}

fn decode_bytes_value<'a>(
    data: &'a [u8],
    cursor: &mut usize,
) -> Result<BencodeRef<'a>, BencodeError> {
    let pos = *cursor;
    let rest = &data[pos..];
    let Some(colon_rel) = rest.iter().position(|&b| b == b':') else {
        return Err(BencodeError::Truncated { position: pos });
    };
    let len_text = std::str::from_utf8(&rest[..colon_rel])
        .map_err(|_| BencodeError::LengthParse { position: pos })?;
    // bencode 规范不允许前导零（除 "0:" 本身）；Python 版 int() 宽容前导零，
    // 为兼容 Python 行为（INDEX §11）这里同样宽容。
    let length: usize = len_text
        .parse()
        .map_err(|_| BencodeError::LengthParse { position: pos })?;
    let start = pos + colon_rel + 1;
    let end = start + length;
    if end > data.len() {
        return Err(BencodeError::Truncated { position: pos });
    }
    *cursor = end;
    Ok(BencodeRef::Bytes(&data[start..end]))
}

/// 把 `BencodeRef` 重新编码为字节（用于测试与 qB 之外的少量场景）。
/// dict 键按 BTreeMap 字节序输出，满足 bencode 规范。
pub fn bencode(value: &BencodeRef<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(value, &mut out);
    out
}

fn encode_value(value: &BencodeRef<'_>, out: &mut Vec<u8>) {
    match value {
        BencodeRef::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        BencodeRef::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        BencodeRef::List(items) => {
            out.push(b'l');
            for item in items {
                encode_value(item, out);
            }
            out.push(b'e');
        }
        BencodeRef::Dict(map, _) => {
            out.push(b'd');
            for (key, value) in map {
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key);
                encode_value(value, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Python tests/test_core.py::test_bdecode_types 完全对齐（INDEX §11）
    #[test]
    fn bdecode_types_match_python_matrix() {
        assert_eq!(bdecode(b"i42e").unwrap(), BencodeRef::Int(42));
        assert_eq!(bdecode(b"i-7e").unwrap(), BencodeRef::Int(-7));
        assert_eq!(
            bdecode(b"li1ei2ei3ee").unwrap(),
            BencodeRef::List(vec![
                BencodeRef::Int(1),
                BencodeRef::Int(2),
                BencodeRef::Int(3)
            ])
        );
        assert_eq!(bdecode(b"le").unwrap(), BencodeRef::List(vec![]));
        let d = bdecode(b"d3:foo3:bare").unwrap();
        match d {
            BencodeRef::Dict(map, _) => {
                assert_eq!(
                    map.get(b"foo".as_slice()),
                    Some(&BencodeRef::Bytes(b"bar".as_slice()))
                );
            }
            _ => panic!("expected dict"),
        }
        assert_eq!(bdecode(b"4:spam").unwrap(), BencodeRef::Bytes(b"spam"));
    }

    /// Python 版 `_decode` 对截断数据会异常（index 越界/None 解包），parse_bytes 捕获后
    /// 返回 None。Rust 版这里返回 Err，由 parser 层统一转 Option。
    #[test]
    fn bdecode_truncated_is_error() {
        assert!(bdecode(b"d3:foo").is_err());
        assert!(bdecode(b"i42").is_err());
        assert!(bdecode(b"l").is_err());
    }

    /// golden 字节串来自 Python tests/helpers.py（手写外部值，非实现推导）
    #[test]
    fn bencode_roundtrip_matches_golden_bytes() {
        let golden = b"d6:lengthi1234e4:name8:test.dat12:piece lengthi65536e6:pieces20:\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0e".to_vec();
        let decoded = bdecode(&golden).unwrap();
        match &decoded {
            // golden 本身就是一个顶层 dict（Python helpers 构造的是 info dict 内容）
            BencodeRef::Dict(map, range) => {
                // 顶层 dict 的 raw 区间必须完整覆盖 golden 字节
                assert_eq!(&golden[range.clone()], golden.as_slice());
                assert!(map.contains_key(b"pieces".as_slice()));
                assert!(map.contains_key(b"piece length".as_slice()));
            }
            _ => panic!("expected dict"),
        }
        assert_eq!(bencode(&decoded), golden);
    }

    #[test]
    fn bencode_dict_keys_sorted_bytewise() {
        // Python 版按 sorted(keys)（bytes 序）编码
        let decoded = bdecode(b"d1:z1:a1:a1:be").unwrap();
        let re = bencode(&decoded);
        assert_eq!(re, b"d1:a1:b1:z1:ae".to_vec());
    }

    #[test]
    fn empty_dict_and_list_roundtrip() {
        assert_eq!(bdecode(b"de").unwrap(), BencodeRef::Dict(BTreeMap::new(), 0..2));
        assert_eq!(bencode(&bdecode(b"de").unwrap()), b"de".to_vec());
    }
}
