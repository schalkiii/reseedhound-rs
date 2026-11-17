# Python ↔ Rust 交叉审计（2026-11-17）

> 对照双方：`D:/workspace/SeedHound`（Python 版，HEAD 1cf73a4）与本仓库（Rust 版）。
> 目的：把 Rust 开发/实机调试中发现的问题在 Python 侧复查修补，同时反向找 Rust 缺口，
> 并对照测试覆盖。**验证约束：PT 站点解封前不做真实站点验证（麒麟仅少量验证）**，
> 本轮只跑单测（Rust 89 / Python 192）。

## 1. Python → Rust：Rust 缺口（已补）

| 缺口 | Python 依据 | Rust 修补 |
|---|---|---|
| **qB `torrents/add` 返回值语义** | `QbittorrentDownloader._is_add_success`：qB 5.2+（WebAPI v2.14.0）返回 JSON 计数（success/failure/pending），旧版才是 `"Ok."`，一条没加进去时是 **409** | `downloader.rs::is_qb_add_success`：统一判定 409/JSON 计数/"Ok."/空体；`add_torrent_bytes` 与 `qb_add_torrent_file` 都改为读响应体后判定 + 单测 6 组 |
| 影响 | 只看 HTTP 2xx 会把「200 + failure_count=1」记成**辅种成功**——配额消耗了、qB 里没有，正是麒麟个案的账目差形态 | 与 Python 语义对齐 |

## 2. Rust → Python：Python 缺口（已补）

| 缺口 | Rust 依据 | Python 修补 |
|---|---|---|
| CookieCloud `aes-128-cbc-fixed`（0.3.0+） | `cookiecloud.rs` 按 `crypto_type` 双算法 + `Salted__` 头自动识别 | `cookiecloud.py::_decrypt_fixed_iv`（key=口令 16 字节、IV 全 0）+ `crypto_type` 分发；非法长度仍报「不支持的加密格式」 |
| 阶段2/3 无进度日志 | Rust 已有 `[阶段1] 解析进度 n/total`、`[阶段2] 站点进度 n/total`、`[阶段3] 进度 n/total` | `seeder.py` 同口径补齐（阶段2 每站点完成一行、阶段3 每 50 颗） |
| add 失败静默 | Rust 补过「添加失败（已消耗 XX 下载配额）」warn | `seeder.py` 同口径补齐（此前只在 downloader 层有零散 warning） |

## 3. 两边一致、无需修改（已确认）

- **CookieCloud 口令派生**：Python 一直是 `md5(uuid+'-'+password).hexdigest()[:16]`（正确），
  Rust 移植时写成了 MD5 原始字节，已在 CONFIG-FIX 轮修正——现在两边一致。
- **qB 登录**：Python 用 `qbittorrentapi` 库（自管会话 cookie，兼容 qB 5.x）；
  Rust 手写实现已支持 `SID` / `QBT_SID_<端口>` 两种会话名与 `Cookie` 头。
- **下载超时重试重复消耗站点配额**：两边都是「仅 timeout 重试、每次重试都再次请求
  download.php」，属共同设计行为（已在 docs/PERF 记录），要改需双方同改，暂不动。
- **退出阻塞**：Python 侧 scheduler 无 UI join 阻塞问题（Rust GUI 特有，已修）。
- **配置数值标量**：Rust GUI 表单特有（Python 无 GUI）。

## 4. 测试覆盖对照

| 主题 | Rust（89 测试） | Python（192 测试） | 备注 |
|---|---|---|---|
| CookieCloud legacy 往返 | ✓ | ✓ | 双方夹具均为合成凭据 |
| CookieCloud fixed-IV | ✓（含错口令拒绝） | ✓（本轮新增） | |
| qB add 返回值语义 | ✓（本轮新增 6 组） | ✓ `test_core._is_add_success` | Rust 补齐后对齐 |
| qB 会话 cookie v4/v5 | ✓ | N/A（库自管） | |
| 配置读写（数值标量/引号/缩进） | ✓（GUI 特有） | N/A | |
| 站点查询/重试/节流 | ✓ | ✓ `test_retry_policy/test_network` | |
| 凭据失效提醒 | ✓ | ✓ `test_credential` | |
| HDArea 通道 | ✓ | ✓ `test_hdarea` | |
| 调度器停止/间隔 | ✓ | ✓ `test_scheduler` | |
| 端到端流水线 | 部分（engine 级） | ✓ `test_integration` | Rust 侧集成用例可参考 Python 补 |

## 5. 验证与后续

- Rust：`cargo test --workspace` 89 全过、clippy 0 警告。
- Python：`py -3.13 -m pytest tests/` **192 全过**。
- 站点解封后再做真实验证；**麒麟只做少量验证**（单站点范围、小批量），避免再次触发小时下载上限。
- 遗留共同议题（需要时再做）：每站每轮下载上限配置；下载去重缓存（site torrent_id → info_hash）。
