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

GUI 覆盖 CLI 全部功能：仪表盘可触发立即辅种 / 同步 Cookie / 死种检测 / 生成周报 / 演练模式开关（--dry-run 对应）；设置页四 Tab（下载器/站点/调度与全局/通知与集成）可编辑全部常用配置（飞书/CookieCloud/Jackett/日志/下载器行为开关），站点支持新增与删除。GUI 设计语言（theme.rs 石墨系）：无 emoji 图标、单一雾蓝强调色、背景明度分层（BG/SURFACE/INPUT_BG）、分隔线用 DIVIDER 不堆边框、Tab 下划线激活态、数据用等宽字体+排版层级表达、开关为哑光滑块组件（switch）。

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
- 文档在 `docs/`：RUST-PLAN（里程碑，已收口）、AUDIT（审计记录）、BENCHMARK、GUI-FIX、CONFIG-FIX（配置读写/qB 5.x 实机修复）、PYTHON-INDEX；根 README.md 面向用户

## 当前状态与下一步

- 全部功能移植完成（三轮审计：轮二修 6 项缺陷、轮三补文件日志/weekly-report/SEEDHOUND_MODE/reseed 自动升级；GUI 轮：修 Tab 切换/布局/退出残留，补齐 CLI 功能 GUI 覆盖；审美轮：全 GUI 去模板味重做——石墨色板/下划线 Tab/滑块开关/无框卡片/等宽数据条；审计四轮（AUDIT-REPORT-2026-11-17）：credential_failures 回填/ --site missing 告警 / qb_info_hashes 增量去重 / save_path 传递 / README 补齐；配置实机轮（CONFIG-FIX-2026-11-17）：数值标量显示/写回引号/站点补行缩进/qB 5.x 会话 cookie/CookieCloud 口令派生与 fixed-IV/退出不阻塞/遮罩输入投影/panic 日志），88/88 测试，clippy 0 警告
- 遗留（见 AUDIT-REPORT-2026-11-17 P3 与 AUDIT-2026-11-16 遗留建议）：无（qB 5.x 与 CookieCloud 均已真机验证：qB 5.3.0 登录 204 + `QBT_SID_<端口>`；CookieCloud 真实服务端 86/100 站点同步成功）。站内批次并发 2+熔断、连接池上限调优、站点上移/下移、调度开关实时生效已于 2026-11-17 第二轮全部修复
- 构建提示：gpui 在**非 debug** 配置下由 build.rs 调用 `fxc.exe` 预编译 HLSL（debug 走 d3dcompiler 运行时编译）。无 Windows SDK 的机器只能 `cargo build`（debug），`cargo build --release` 需 `fxc.exe` 或 `GPUI_FXC_PATH`
- git：远程 https://github.com/schalkiii/reseedhound-rs（Public）；v0.2.0 已发布 release（附 CLI/GUI 双 exe，https://github.com/schalkiii/reseedhound-rs/releases/tag/v0.2.0），提交身份 schalkiii@github.com
