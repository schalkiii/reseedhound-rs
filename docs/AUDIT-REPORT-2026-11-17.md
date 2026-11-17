# ReseedHound-rs 审计报告（2026-11-17）

> 审计范围：`reseedhound-rs` 全部 33 个 Rust 源文件 + 5 份 docs + AGENTS.md + 构建配置。
> 对照基线：Python 版（`../ReseedHound`，commit 1cf73a4，见 docs/PYTHON-INDEX.md）。
> 本报告只登记问题与建议，不含改动。执行阶段请配合 software-improvement 的改动纪律。

## 1. 现状描述

- **规模**：4 crate（sh-core / sh-engine / sh-cli / sh-app），33 个 .rs 文件，约 11,297 行。
- **结构**：依赖方向 app/cli → engine → core 单向；core 无网络依赖（rayon 并行解析）；engine 承载网络与三阶段流水线；app 为 GPUI GUI + 托盘。
- **测试**：76 个单测（core 35 / engine 32 / app 9），抽查断言质量良好（真断言 + golden 输出检查，无「跑通即通过」）。E2E 覆盖：weekly-report 5 轮日志 fixture 归因测试。
- **文档**：docs/ 五份（RUST-PLAN / AUDIT-2026-11-16 / BENCHMARK / GUI-FIX / PYTHON-INDEX）+ AGENTS.md，均与代码现状基本一致。
- **规范**：clippy --workspace --all-targets 项目代码 0 警告；.gitignore 完备（target/data/含密钥配置均排除）；git 历史干净（4 commits，身份 schalkiii@github.com）。
- **整体水平**：三轮审计后的成熟度较高，本次审计聚焦移植缺口与残留冗余。

## 2. 问题清单

### P1-1【高】credential_failures 从未填充——飞书「更新凭据」警报永远不触发

- **位置**：`crates/sh-engine/src/seeder.rs` 阶段3（phase_add_torrents，约 735-760 行 is_serious_error 分支）
- **描述**：`ReseedStats::credential_failures`（stats.rs:27）在引擎中无任何写入点。`is_credential_error()`（seeder.rs:66）标着 `#[allow(dead_code)]` 躺着，`CREDENTIAL_ERRORS` 常量除它外无人引用。而 CLI（sh-cli/main.rs:223）与调度器（scheduler.rs:233）都在轮次结束后读取该字段并调用 `send_credential_alert`——读取永远为空，警报永不发出。
- **对照**：Python seeder.py:727-733 在 is_serious_error 分支内显式 `credential_failures.setdefault(site_name, error_reason)`。
- **影响**：passkey/Cookie 失效的用户收不到飞书提醒，功能缺失（移植缺口），且下游读取代码形成「看似已实现」的误导。

### P1-2【高】`--site` 指定不存在站点时静默吞错——Python 会告警 missing

- **位置**：`crates/sh-engine/src/seeder.rs` run() 站点过滤段（约 280-295 行）
- **描述**：Rust 版过滤后 `sites.is_empty()` 才报错退出；部分命中时直接继续，不告警未命中的名字。Python（seeder.py:213-215）会 `logger.warning("以下站点未在配置中找到: ...")` 列出 missing。
- **影响**：用户拼错站点名时（如 `--site mteam,mteam.cn`）只有部分站点被处理，无任何提示，行为与 Python 不对齐。

### P1-3【中】阶段3 缺 qb_info_hashes 增量去重——重复添加已存在种子

- **位置**：`crates/sh-engine/src/seeder.rs` phase_add_torrents（seeder.rs:811 注释自认「暂跳过」）
- **描述**：Python 在 add_one 内解析 torrent 后检查 `new_info_hash in qb_info_hashes`，已存在则直接记 reseed_record 并 continue（跳过下载器 add 调用）。Rust 版把幂等交给 `downloader 层 add 接口`，但 Python 的语义是**跳过 add 请求本身**，省一次 qB API 调用 + 精确计成功数（Python 把已存在也算 succeeded）。Rust 版已存在时 add 会失败 → 计入 failed，统计口径与 Python 不同。
- **影响**：统计数字偏差（succeeded/failed 口径不一致）；每颗已存在种子多一次无效 add 请求。
- **注意**：需先确认 Rust downloader.add_torrent_bytes 对已存在种子的实际返回值再定修复方案。

### P1-4【中】save_path 细粒度落盘缺失——辅种目录可能错位

- **位置**：`crates/sh-engine/src/seeder.rs` phase_add_torrents（同 seeder.rs:811 注释）
- **描述**：Python 添加种子时传 `save_path=dl_info["save_path"]`（源种子在 qB 中的保存路径），保证辅种种子落到同一目录。Rust 版 add_torrent_bytes 不带 save_path，由 qB 默认目录接管。
- **影响**：辅种种子可能落到 qB 默认下载目录而非源种子目录，做种路径错位（对已做种内容 hash 校验可能失败）。**这是已知遗留**（AUDIT-2026-11-16 未列入，属于移植时注释自认的缺口），建议随 P1-3 一并补齐。
- **先验证**：downloader.rs 是否有 get_torrent_info/save_path 通道，缺失则需补 API。

### P1-5【中】README.md 缺失——发布 blocker

- **位置**：仓库根目录
- **描述**：无 README.md。AGENTS.md 面向 Agent，docs/ 面向过程记录，GitHub 发布（release）时仓库首页空白。
- **影响**：本次「push 打 tag 发 release」的直接障碍；新用户无法了解项目用途/构建方式。

### P2-1【低】dead_code 函数与重复定义

- **位置**：
  - `seeder.rs:65-68` `is_credential_error`（#[allow(dead_code)]）——P1-1 修复后应摘掉 allow 并使用
  - `settings.rs:630-634` / `setup_wizard.rs:287-291` `truncate_chars` 两份同构 dead_code 副本
  - `config_store.rs:384` `config_exists`（#[allow(dead_code)]，main.rs 用 config_path.exists() 自行判断）
- **描述**：三处 dead_code。truncate_chars 是历史遗留（placeholder 截断），从未被调用。
- **影响**：违反「严禁冗余」规范；误导读者以为有截断行为。

### P2-2【低】RUST-PLAN.md 里程碑段落过期

- **位置**：`docs/RUST-PLAN.md` M1-M6 段
- **描述**：M6 写「64 测试全绿」，现 76；计划性文档在完成后未收口（M 段还保持「计划中」语气）。技术选型表写「gpui-ce 0.3（社区版）」，实际 Cargo.toml 依赖 gpui 0.2.2（GUI-FIX 有记录但 PLAN 未改）。
- **影响**：文档与实现矛盾；后续接手者会被选型表误导。

### P2-3【低】Cargo.toml workspace 缺元数据（发布配套）

- **位置**：根 Cargo.toml / 各 crate Cargo.toml
- **描述**：无 `description` / `license` / `repository` 字段（crates.io 发布不需要，但 GitHub release 归档与 `cargo metadata` 工具链消费时缺失）。
- **影响**：低；发布流程非必需。

### P3-1【低】GUI 遗留确认（不修，记录在案）

- 设置页无站点行上移/下移；调度开关变更需重启生效（保存时已有提示）。均已记录在 AGENTS.md，属产品决策范畴。

## 3. 改进建议

### A1（修 P1-1）凭据失败回填 credential_failures

- **做法**：在 phase_add_torrents spawn 内 is_serious_error 分支，对照 Python 语义补 `credential_failures` 记录。stats 结构跨 spawn 收集：仿照 `site_failed` 聚合模式，把 `(site_name, reason)` 经返回值聚合回填 stats.credential_failures（setdefault 语义 = 首个凭据原因）。同时摘掉 is_credential_error 的 #[allow(dead_code)]。
- **文件**：`crates/sh-engine/src/seeder.rs`
- **验证**：新增单测覆盖 reason 分类；手动跑一轮含 401 的站点确认飞书警报路径。最小验证：`cargo test -p sh-engine`。

### A2（修 P1-2）--site missing 告警

- **做法**：run() 站点过滤段补 missing 计算与 `tracing::warn!("以下站点未在配置中找到: ...")`（对齐 Python 文案）。
- **文件**：`crates/sh-engine/src/seeder.rs`
- **验证**：无（纯日志）；可选用现有 config fixture 写过滤单测。
- **可并行**：与 A1 同文件不同函数，但建议串行避免冲突。

### A3（修 P1-3 + P1-4）qb_info_hashes 增量去重 + save_path

- **做法**：phase_add_torrents 开头取一次 `qb_get_all_info_hashes()`（downloader 已有该 API，jackett 阶段在用）；spawn 内解析 torrent 后、add 前检查 `parsed.info_hash in qb_info_hashes`，命中则记 reseed_record + 视为成功 continue。save_path：downloader 补 `qb_torrent_info(info_hash) -> Option<save_path>`（若已有 get_torrent_info 则直接用），add_torrent_bytes 增加可选 save_path 参数（None = 不发送）。
- **文件**：`crates/sh-engine/src/seeder.rs`、`crates/sh-engine/src/downloader.rs`
- **验证**：`cargo test -p sh-engine`；downloader 层单测（若可离线 mock）。**动手前先读 downloader.rs 确认现有 API 面**。
- **难度**：中。涉及 qB API 调用时序，需谨慎对齐 Python（Python 取 dl_info 还含 state 检查——Rust 已有 qb_torrent_status 前置检查，语义覆盖）。

### A4（修 P1-5）补 README.md

- **做法**：新建根 README.md，内容：项目一句话定位（PT 辅种引擎 Rust 版，Python ReseedHound 的全量重写）、双 exe 用法（reseedhound.exe CLI / reseedhound-gui.exe GUI）、配置文件说明（config.yaml/sites.yaml，模板在 crates/sh-app/assets/）、构建（cargo build --release）、CLI 命令面、文档索引（docs/）。控制在 80 行内，不复制 AGENTS.md。
- **文件**：`README.md`（新建）
- **验证**：通读无矛盾；链接存在。
- **可并行**：独立文件。

### A5（修 P2-1）死代码清理

- **做法**：删除 settings.rs / setup_wizard.rs 两份 truncate_chars 及其 `use unicode_segmentation`（若仅它使用）；删除 config_store.rs config_exists（先确认全仓无引用）。
- **文件**：`crates/sh-app/src/settings.rs`、`setup_wizard.rs`、`config_store.rs`
- **验证**：`cargo build -p sh-app` + `cargo test -p sh-app` 零警告。
- **可并行**：独立于引擎侧改动。

### A6（修 P2-2）RUST-PLAN 收口

- **做法**：M1-M6 段头加「已完成」标注与最终数字（76 测试）；技术选型表 gpui 行改为实际依赖（gpui 0.2.2，见 GUI-FIX）。不重写历史叙述。
- **文件**：`docs/RUST-PLAN.md`
- **验证**：与 Cargo.toml / AGENTS.md 数字一致。
- **可并行**：独立文件。

## 4. 优先级与并行性

| 优先级 | 条目 | 并行性 |
|---|---|---|
| 高 | A1 凭据失败回填 | 引擎侧，与 A2 串行（同文件） |
| 高 | A2 --site missing 告警 | 接 A1 后做 |
| 高 | A3 qb 去重 + save_path | 引擎侧，A1/A2 完成后做（同文件且语义相邻） |
| 高 | A4 README | **可并行**（独立文件） |
| 中 | A5 死代码清理 | **可并行**（app 侧，与引擎侧无文件重叠） |
| 中 | A6 RUST-PLAN 收口 | **可并行**（独立文件） |

执行分组建议：
- **组一（引擎侧，串行）**：A1 → A2 → A3，每步 `cargo test -p sh-engine` 全绿再进。
- **组二（可并行）**：A4 / A5 / A6 互不重叠，可任意穿插。

全部完成后：`cargo test --workspace` + `cargo clippy --workspace --all-targets` + release 构建全绿，然后进入发布流程（tag + release）。
