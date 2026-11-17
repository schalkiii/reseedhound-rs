# ReseedHound RS

PT 辅种引擎——[ReseedHound](../ReseedHound)（Python）的全量 Rust 重写。扫描本地种子目录，
跨 PT 站点匹配同内容种子（pieces_hash / HDArea info_hash / Jackett 三通道），自动添加到
qBittorrent / Transmission 做种。单二进制、原生 GUI、Windows 托盘常驻。

## 下载与运行

Release 附两个 exe，放同一目录：

```
reseedhound-gui.exe    # GUI（无配置时进首启向导）
reseedhound.exe        # CLI（无参数 = 启动 GUI）
```

`config.yaml` / `sites.yaml` 放 exe 同目录（模板见 `crates/sh-app/assets/*.example.yaml`，
或首启向导自动生成）。文件日志 `logs/reseed.log`（凭据脱敏 + 按轮次裁剪）。

## CLI 命令面

```
reseedhound reseed          # 单次辅种（scheduler.enabled=true 时自动切定时）
reseedhound schedule        # 定时调度
reseedhound sync-cookies    # 从 CookieCloud 同步站点 Cookie
reseedhound check-dead      # 对照 savept.icu 死亡名单（--apply 写入 sites.yaml）
reseedhound weekly-report   # 日志周报生成 + 远期日志清理（--clean --apply）
reseedhound gui             # 启动图形界面 + 托盘
```

常用参数（全局）：`--dry-run` 演练模式（只查询不添加）、`--site <名称>` 站点过滤、
`--no-feishu` 跳过飞书推送、`--mode <pieces_hash|jackett|both>`、`--config` / `--sites-path`。

无子命令时读 `SEEDHOUND_MODE` 环境变量（Docker 场景）。

## 构建

```
cargo build --release
```

产物：`target/release/reseedhound.exe` + `target/release/reseedhound-gui.exe`。
GUI 基于 gpui 0.2（DirectX 渲染），仅支持 Windows。

## 架构

```
crates/
├── sh-core/    # 纯逻辑：bencode 解析 / info_hash / 配置 / SQLite 缓存 / 报告 / 脱敏日志
├── sh-engine/  # 网络与流水线：三阶段引擎 / site_client / hdarea / jackett / downloader
│               #   / cookiecloud / savept / feishu / scheduler / weekly_report
├── sh-cli/     # 原生 CLI 入口
└── sh-app/     # GPUI 仪表盘 + 首启向导 + 设置页 + Windows 托盘
```

依赖方向：app/cli → engine → core（core 无网络依赖）。

## 文档

- [docs/PYTHON-INDEX.md](docs/PYTHON-INDEX.md) — Python 版行为规格基线
- [docs/RUST-PLAN.md](docs/RUST-PLAN.md) — 重写计划与里程碑
- [docs/AUDIT-REPORT-2026-11-17.md](docs/AUDIT-REPORT-2026-11-17.md) — 最近一轮审计
- [docs/BENCHMARK.md](docs/BENCHMARK.md) — 与 Python 版性能对比
