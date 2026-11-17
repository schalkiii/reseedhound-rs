# ReseedHound-rs

PT 站辅种引擎的 Rust 全量重写（原 Python 版见 `D:/workspace/ReseedHound/ReseedHound`，
源码索引在 `docs/PYTHON-INDEX.md`）。单二进制原生 exe：CLI + GPUI 图形界面 + Windows 托盘。

## 怎么跑

```bash
cargo build --release          # 构建（产物 target/release/）
cargo test --workspace         # 76 个测试
cargo clippy --workspace       # 项目代码应保持 0 警告
./target/release/reseedhound-gui.exe    # GUI（无配置时进首启向导）
./target/release/reseedhound.exe        # CLI（无参数=启动 GUI）
```

配置：`config.yaml` / `sites.yaml` 放 exe 同目录（模板见 `crates/sh-app/assets/*.example.yaml`）。

CLI 命令面：`reseed`（scheduler.enabled=true 时自动切定时）/ `schedule` / `sync-cookies` / `check-dead` / `weekly-report`（周报+日志清理）/ `gui`。无子命令时读 `SEEDHOUND_MODE` 环境变量（Docker 场景）。文件日志 `logs/reseed.log`（脱敏+按轮次裁剪，对齐 Python logger.py）。

GUI 覆盖 CLI 全部功能：仪表盘可触发立即辅种 / 同步 Cookie / 死种检测 / 生成周报 / 演练模式开关（--dry-run 对应）；设置页四 Tab（下载器/站点/调度与全局/通知与集成）可编辑全部常用配置（飞书/CookieCloud/Jackett/日志/下载器行为开关），站点支持新增与删除。

## 技术栈

- Cargo workspace 4 crate：`sh-core`（纯逻辑）→ `sh-engine`（网络/流水线）→ `sh-cli` / `sh-app`
- tokio 异步 + reqwest(rustls) + rusqlite + rayon（种子扫描并行）
- GUI：gpui 0.2（DirectX 渲染）；托盘：tray-icon 0.26 独立线程消息泵
- 严禁引入 Python 依赖；与 Python 版行为对齐是硬约束（对照细节见 docs/AUDIT-2026-11-16.md）

## 目录与约定

- `crates/sh-core`：bencode/hash/config/cache/report/redact/log_setup（文件日志+脱敏+裁剪）
- `crates/sh-engine`：site_client/downloader/hdarea/jackett/cookiecloud/savept/feishu/seeder/scheduler/weekly_report
- `crates/sh-app`：main（编排）/setup_wizard（首启向导）/settings（配置编辑）/dashboard/text_input/theme/tray
- 命名与结构遵循：描述性命名、集中参数管理、精确类型、拒绝冗余（对齐用户 RTL 代码规范）
- 文档在 `docs/`：RUST-PLAN（里程碑）、AUDIT（三轮审计）、BENCHMARK、GUI-FIX、PYTHON-INDEX

## 当前状态与下一步

- 全部功能移植完成（三轮审计：轮二修 6 项缺陷、轮三补文件日志/weekly-report/SEEDHOUND_MODE/reseed 自动升级；GUI 轮：修 Tab 切换/布局/退出残留，补齐 CLI 功能 GUI 覆盖），76/76 测试，clippy 0 警告
- 已知遗留（见 AUDIT「遗留建议」）：站内批次并发 2+熔断、连接池上限调优、qB 5.x 真机验证、GUI 调度开关变更需重启生效
- git：v0.1.0 已打 tag（提交身份 schalkiii@github.com）
