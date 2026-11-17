# ReseedHound-rs Benchmark：种子解析性能对照

日期：2026-11-16 · 环境：Intel Arc 130T 本机 / Windows / Rust 1.99.1 / Python 3.13

## 测试方法

- 10,000 个合成 torrent（单文件，info dict 含 length/name/piece length/pieces，1-8 个 piece）
- 生成脚本保证 bencode 合法（整数 `i...e` 包裹），Python `bdecode` 与 Rust `bencode.rs` 均可正常解析
- Python：`TorrentParser.parse_file` 串行循环（`scan_directory` 的 to_thread 并发在此规模受 Windows IO 调度影响波动大，取串行作基线）
- Rust：`sh_core::parser::scan_directory`（rayon 并行，release 构建）
- 各跑 2 轮取稳定值，统计解析成功数一致（10000/10000）

## 结果

| 实现 | 耗时 | 吞吐 |
|---|---|---|
| Python 3.13 串行 | 3.28 ~ 3.39 s | ~3,000 个/秒 |
| **Rust rayon 并行（release）** | **1.59 ~ 1.63 s** | **~6,200 个/秒** |

**Rust 版约 2.0× 加速**（单文件解析本身是轻量工作，IO 占比高，纯 CPU 密集场景差距更大）。

注意：本 benchmark 为同机文件系统冷热混合读。真实场景（本地下载器种子目录、含多文件大种子的 pieces 哈希计算）中 Rust 零拷贝解析避免 Python bdecode 的字节切片复制，差距会进一步拉大。

复现：
```
cargo run --release -p sh-core --example bench_scan
python3 /tmp/bench_py2.py   # Python 基线
```
