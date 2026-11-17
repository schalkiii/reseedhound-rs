# ReseedHound-rs 全量 Rust 重写计划

> 规格基线: docs/PYTHON-INDEX.md (Python commit 1cf73a4)
> 目标: 单二进制原生 exe + GPUI 图形界面/仪表盘 + Windows 托盘常驻

## 0. 技术选型（已验证）

| 项 | 选择 | 理由 |
|---|---|---|
| 异步运行时 | tokio (full) | 标准选择 |
| HTTP | reqwest + rustls-tls | `pool_max_idle_per_host(0)` 等价 Python force_close |
| GUI | **gpui-ce 0.3** (社区版) | Windows Win32+DirectX；Rust≥1.95（本机 1.99.1）；crates.io 可直接依赖，避免 Zed git pin 问题 |
| 托盘 | tray-icon + muda (菜单) | Windows 原生托盘，成熟 |
| SQLite | rusqlite (bundled) | schema 与 Python 版完全兼容 |
| CLI | clap 4 derive | 子命令对齐 Python 版 |
| 日志 | tracing + tracing-subscriber | 自定义 Layer 实现凭据脱敏 |
| 并行解析 | rayon | 替代 Python 线程池 |
| bencode | 自研零拷贝游标 | info_hash 需要原始字节区间 |
| 序列化 | serde + serde_json + serde_yaml | |
| 加密 | aes, md-5, base64, hmac, sha1, sha2 | CookieCloud/飞书签名 |
| XML | quick-xml | Torznab |
| 错误 | thiserror (库) + anyhow (bin) | |

## 1. Workspace 结构

```
reseedhound-rs/
├── Cargo.toml            # workspace
├── crates/
│   ├── sh-core/          # bencode/hash/config/cache/报告（无网络，纯逻辑，全单测）
│   ├── sh-engine/        # 三阶段流水线/site_client/hdarea/jackett/downloader/feishu
│   │                     #   /cookiecloud/savept/scheduler（tokio 异步）
│   ├── sh-cli/           # 原生 exe 入口（reseed/schedule/sync-cookies/check-dead）
│   └── sh-app/           # GPUI 仪表盘 + Windows 托盘（gui 子命令默认）
├── docs/                 # PYTHON-INDEX.md / RUST-PLAN.md / AUDIT / 阶段记录
└── tests/                # 跨 crate 集成测试（golden 对比）
```

依赖方向: app/cli → engine → core。core 不依赖 tokio（解析用 rayon）。

## 实施进度（2026-11-16 更新）

- ✅ M1 sh-core：bencode 零拷贝/parser(golden)/config/cache/stats/report/site_table
- ✅ M2 sh-engine：site_client/downloader/hdarea/jackett/feishu/seeder/scheduler
  + **cookiecloud**（AES-256-CBC EVP_BytesToKey 解密 + 域名匹配 + 写回 sites.yaml + 调度器每轮前置同步）
  + **savept**（分页拉取死亡名单 + 三级匹配 high/medium + check-dead --apply 文本行禁用写回）
- ✅ M3 sh-cli：clap 4 子命令（reseed/schedule/sync-cookies/check-dead/gui）
- ✅ M4+M5 合并为 sh-app：GPUI 仪表盘（指标卡+日志面板+立即辅种按钮）+ tray-icon 托盘
- ✅ M6 验证/优化/交付：64 测试全绿（sh-core 31 + sh-engine 28 + sh-app 5）；clippy 项目代码 0 警告；
  benchmark 10k 种子 Python 3.28s vs Rust 1.59s（2.0×，见 docs/BENCHMARK.md）；
  release 产物 reseedhound.exe 9.1MB / reseedhound-gui.exe 14.5MB；
  tag v0.1.0 待用户确认（项目目录尚未 git init）
- ✅ 二轮深化（GUI 大改版 + 审计修复，2026-11-16 晚）：
  + GUI 7 模块重构（theme/text_input/config_store/setup_wizard/dashboard/settings/tray）：
    首启向导、配置编辑页（下载器/调度/站点增删改）、视觉体系升级
  + 审计修复 6 项：--mode 死参数接通、HDArea dl_token 短效直链传递、
    signin 匹配、死种持久化接通（record_dead）、源种子前置状态检查、
    stats_shared 死代码删除（详见 docs/AUDIT-2026-11-16.md 二轮章节）

## 2. 里程碑

### M1 — sh-core（纯逻辑层）
- `bencode`: 游标解析 + `RawValue`（记录字节区间）；`parse_torrent()` 返回 info 原始字节引用
- `hash`: sha1_hex / info_hash(原始字节区间直接 SHA1) / pieces_hash
- `config`: serde 反序列化 + DEFAULTS 合并 + 未知字段告警（两遍：Value 预扫描→强类型）
- `cache`: rusqlite 三表（schema 兼容老库）
- `report`: 文本报告 + 站点表（display_width 中日韩对齐）
- `stats`: ReseedStats（GUI 仪表盘共用数据结构，含 serde 序列化给 GUI）
- **验收**: golden 三值通过 + bdecode 行为矩阵通过 + 老库兼容测试

### M2 — sh-engine（网络与流水线）
- `site_client`: 限流/退避/错误分类（reason 枚举全量对齐 INDEX §4.3）/死种缓存/HTML 检测
- `downloader`: trait Downloader + qB(手写 REST, 三态判定, pausedUP/stoppedUP) + TR(JSON-RPC)
- `hdarea` / `jackett` / `cookiecloud` / `savept` / `feishu`
- `seeder`: 三阶段流水线 + 统计
- `runner`: run_once / `scheduler`: 定时循环
- **验收**: 引擎层单测（mock HTTP server, axum）+ 与 Python 版错误分类矩阵逐条对照

### M3 — sh-cli
- clap 子命令 4 个 + SEEDHOUND_MODE
- **验收**: `reseedhound.exe --help` / dry-run 冒烟

### M4 — sh-tray（Windows 托盘）
- tray-icon 托盘 + 右键菜单（立即辅种/打开仪表盘/暂停调度/退出）
- 后台跑 scheduler；单实例互斥（named mutex）；开机自启（注册表 Run 键，可选项）
- **验收**: 托盘常驻 + 定时轮次 + 菜单触发即时辅种

### M5 — sh-gui（GPUI 仪表盘）
- 布局: 顶部状态条(运行状态/下轮倒计时) + 左侧 8 指标卡 + 右下站点表(四档状态灯) + 底部日志流
- 数据通道: mpsc → GPUI Entity；引擎在独立 tokio 线程跑，GUI 只读快照
- 与托盘整合: 托盘是主进程，GUI 窗口可开关
- **验收**: 仪表盘实时刷新一轮 dry-run 的进度与统计

### M6 — 验证/优化/交付
- cargo test 全绿（目标 ≥60 测试覆盖核心行为矩阵）
- benchmark: 生成 10k 合成种子，Python bdecode vs Rust 解析耗时对照
- release 构建: 体积/内存/启动时间测量，写入 docs/BENCHMARK.md
- 行为对照清单: INDEX §4-§10 逐项勾验
- 打 tag v0.1.0

## 3. 风险与降级预案

| 风险 | 概率 | 预案 |
|---|---|---|
| gpui-ce Windows 构建失败/依赖地狱 | 中 | M5 前先做 10 分钟 PoC（空窗口）；失败则降级 egui（仍满足"现代 GUI"要求，但需用户确认） |
| qB 5.x API 差异 | 低 | Python 版已踩坑（三态判定/状态名），规格已写入 INDEX |
| 老库 schema 差异 | 低 | 建表全用 IF NOT EXISTS + 唯一索引首次升级去重逻辑保留 |
| tray-icon 事件循环与 GPUI/tokio 冲突 | 中 | 托盘运行在主线程 + winit event loop proxy；引擎在 tokio 独立线程，经 channel 通信 |
| 单会话无法完成全部 M1-M6 | 高 | 严格按里程碑顺序交付，每个 M 结束都是可编译可用状态；文档记录进度 |

## 4. 行为对齐原则

1. reason 字符串、统计口径、默认值、SQLite schema 与 Python 版**逐字对齐**（INDEX 为准）
2. 日志文案可重写，但脱敏规则（passkey/apikey/dl_token/X-API-Key）必须保留
3. 配置文件格式完全兼容：用户的 config.yaml / sites.yaml 无需任何修改即可切换到 Rust 版
4. CLI 参数名与 Python 版一致（--dry-run/--site/--mode/--config/--sites-path）
