# SeedHound (Python) 源码索引 — Rust 重写规格基线

> 基线 commit: `1cf73a4` (origin/main, 2026-11-16 拉取)
> 本文档是 Rust 重写的**行为规格单一来源**。所有语义（限流/重试/跳过/统计口径）以本文档为准，
> 与 Python 源码冲突时先查源码再更新本文档。
> Python 明文参考副本: `C:/Users/qi.shao/WorkBuddy/2026-11-12-10-33-00/py-ref/`

## 1. 模块清单（20 个业务模块，~2900 行）

| 模块 | 行数 | 职责 | Rust 对应物 |
|---|---|---|---|
| `run.py` | 291 | CLI 入口（argparse 4 子命令 + SEEDHOUND_MODE 环境变量） | `clap` derive 子命令 |
| `src/runner.py` | 45 | run_once(): 单轮辅种唯一实现（一次性+定时共用） | `engine::runner` |
| `src/scheduler.py` | 136 | 定时调度器，SIGINT/SIGTERM 优雅退出 | `tokio::select!` + ctrl_c |
| `src/engine/seeder.py` | 844 | 三阶段流水线主控 + ReseedStats 统计 | `engine::seeder` |
| `src/engine/parser.py` | 101 | torrent 解析 + scan_directory（线程池并发） | rayon 并行迭代 |
| `src/engine/downloader.py` | 304 | qB/TR 下载器抽象（DownloaderBase） | trait + reqwest 实现 |
| `src/engine/jackett.py` | 435 | Jackett Torznab 搜索 + 尺寸过滤 + dl 缓存 | `engine::jackett` |
| `src/network/client.py` | 575 | SiteClient: 限流/重试/退避/死种缓存/HTML 错误识别 | `net::site_client` |
| `src/network/hdarea.py` | 179 | HDArea REST info_hash 通道（≤200/批，1.1s 节流） | `net::hdarea` |
| `src/network/cookiecloud.py` | 112 | CookieCloud 拉取 + OpenSSL EVP_BytesToKey 解密 | `aes` + `md-5` |
| `src/network/savept.py` | 312 | savept.icu 死亡名单 + 域名对照 + 写回禁用 | `net::savept` |
| `src/notify/feishu.py` | 193 | 飞书卡片推送 + HMAC-SHA256 签名 | `notify::feishu` |
| `src/report/generator.py` | 158 | 控制台文本报告（中文对齐 padding） | `report::text` |
| `src/report/site_table.py` | 74 | 站点行/四档状态灯/排序（两报告共用） | `report::site_table` |
| `src/storage/config.py` | 305 | Config + DEFAULTS + 未知字段告警 | `config` (serde) |
| `src/storage/cache.py` | 178 | SQLite 三表：pieces_cache / reseed_history / dead_torrents | `rusqlite` |
| `src/cookie_sync.py` | 96 | CookieCloud → sites.yaml 写回（保留注释顺序→文本处理） | 文本行处理 |
| `src/utils/_bencode.py` | 56 | 纯 Python bencode 编解码 | 零拷贝游标实现 |
| `src/utils/hash.py` | 14 | SHA1 hex(info_hash / pieces_hash) | `sha1` crate |
| `src/utils/logger.py` | 109 | 日志 + 凭据脱敏过滤器 + 日志裁剪 | `tracing` + Layer 脱敏 |

## 2. 核心数据流（三阶段流水线）

```
阶段1 扫描   torrent_dir/*.torrent（非递归）
             → 并发解析（100 并发线程池）
             → filter_uncached（SQLite 增量）→ add_batch 落库
             → pieces_hash 去重（首次出现保留）
阶段2 查询   对每个启用站点（全局 concurrency 信号量）:
               通用通道: 批量 POST {passkey, pieces_hash[≤100]} → api_url
               HDArea:   POST /api/v1/torrents/hashes {hashes[≤200]} + X-API-Key
             → 查询前三层过滤: ① tracker 域名排除（同站种子）
               ② reseed_history 排除（已辅种过）③ 下载后 info_hash 去重兜底
             → match_map: pieces_hash → [(site, torrent_id, url)]
阶段3 添加   对每个匹配种子（20 并发信号量）:
               排除 no_access_sites（严重错误≥3 或超时≥30）
               下载 dl_url（HDArea 用短效链接，通用用 passkey 拼）
               → 403 时 HDArea 用 apikey 直链重试一次
               → 解析校验 → pieces_hash 必须一致（通用通道）
               → qB 中已有 info_hash 则记 reseed_history 跳过
               → add_torrent(save_path=源种子路径, skip_hash_check, paused=!auto_start, tag)
               → auto_start 时 _ensure_started: 最多3次×0.6s 检查+resume
             → 每次下载后 sleep 0.3s
```

## 3. 配置规格

### 3.1 config.yaml（DEFAULTS 单一来源）

```yaml
global:     {concurrency: 20, batch_size: 100, api_timeout: 60, download_timeout: 15,
             download_interval: 5.0, retry_count: 2, retry_delay: 2.0, mode: pieces_hash}
db:         {path: data/cache.db}
log:        {level: INFO, file: logs/reseed.log}
scheduler:  {enabled: false, interval_minutes: 360, startup_delay: 0, run_on_start: true}
downloader: {torrent_dir: "", skip_hash_check: true, auto_start: true, tag: SeedHound}
            # + 必填 type/host/port/username/password；双下载器: source/destination 子块
jackett:    {enabled: false, url: http://localhost:9117, api_key: "", concurrency: 5,
             search_interval: 1.0, timeout: 30}   # retry_count/retry_delay 可继承 global
cookiecloud:{enabled: false, url: http://127.0.0.1:8082/cookie, uuid: "", password: ""}
hdarea:     {request_interval: 1.1, concurrency: 2}
savept:     {url: https://savept.icu, timeout: 20.0}
notify.feishu: {webhook_url: "", secret: ""}
```

### 3.2 sites.yaml 站点字段

`name, url, api_url, passkey, api_type(默认空=通用; "hdarea"), api_key, enabled/enable(enable 优先), cookie/cookies(cookie 优先), download_interval(按域名覆盖全局)`

- 站点准入: passkey 或 api_key 至少一项，否则**静默跳过**
- `--site` 过滤: 名称逗号分隔，大小写不敏感；未匹配报错并列出可用站点

### 3.3 未知字段告警（不阻断）

KNOWN_KEYS 之外的字段收集后统一 warning，含 `downloader.{source,destination}.*` 与 `notify.feishu.*` 嵌套校验。**Rust 版保留此行为**（serde `deny_unknown_fields` 会阻断，需要自定义告警层）。

## 4. 网络行为规范（SiteClient）

### 4.1 连接层
- `TCPConnector(limit=concurrency*2, limit_per_host=6, ttl_dns_cache=300, force_close=True)`
  → Rust: reqwest `pool_max_idle_per_host(0)`（等价 force_close）+ `pool_idle_timeout(0)`
- 默认头: `User-Agent: SeedHound/2.0`, `Accept: application/json`
- 站点 Cookie 预置: sites.yaml 的 cookie 串按 `;` 拆分注入 cookie jar

### 4.2 下载限流（get_bytes）
- 按 host 锁 + `host_interval`（站点级 download_interval 覆盖全局 5.0s）
- 退避: `host_backoff` 默认 1.0，检测到限流提示后 `max(backoff, limit_delay/host_interval)`
- 等待公式: `wait = interval*backoff - (now - last_request)`，锁内 sleep

### 4.3 下载结果分类（错误 reason 字符串，全量枚举）
```
""                 成功
"auth_redirect"    3xx → Location 含 "login"（标记死种）
"rate_limit"       HTTP 限流状态码 + 响应体含限流文案（解析"X分钟后/X秒后"数字，最低 10s）
"http_{status}"    其他非 200（301-308 未含 login 的重定向也归此类）
"html_permission_denied" / "html_torrent_deleted" / "html_not_found" /
"html_auth_required" / "html_forbidden" / "html_unknown"
                   200 但返回 HTML，正文匹配中文/英文关键词（标记死种，除 unknown）
"too_short"        数据 < 20 字节
"timeout"          请求超时
"connection_error" 网络/解析异常
"dead_cached"      死种缓存命中（try_download 前置短路）
```
HTML 检测: 前 200 字节含 `<html` / `<!doctype`（lowercase）
限流文案关键词: `频繁 / rate limit / too many / 请稍后 / 请稍候`（前 500 字节）
限流数字: `(\d+)\s*分钟后`→×60s、`(\d+)\s*秒后`、`wait (\d+) minute`、`try again in (\d+) second`、`retry after (\d+) second`；无数字默认 60s；上限 10000 字节/500 字节

### 4.4 try_download 重试
- 死种缓存前置短路 → `dead_cached`
- 最多 retry_count(2)+1 次尝试；**仅 timeout 重试**，其他 reason 直接返回
- 重试延迟: `retry_delay * (attempt+1)` 递增

### 4.5 post_json（站点 API 查询）
- retries 默认 1；429 → Retry-After 头（非数字回退 5s）等待后重试，**不计入** attempts 循环上限之外的额外次
- 硬失败不重试: HTTP {401,403,404,405,410}、DNS 解析失败、证书错误、200 但非 JSON（html_invalid_response）
- 非硬失败按 `retry_delay*(attempt+1)` 退避

### 4.6 query_site_batched
- 每站点批内 2 并发（内层信号量）+ 连续失败 ≥5 → 置 site_dead 跳过剩余批次
- 响应 `data: {pieces_hash: torrent_id}` 映射；跳过键名 "passkey"/"pieces_hash" 的意外项

### 4.7 HDArea 通道
- `api_type: hdarea`；X-API-Key 头；批 ≤200（硬上限）；全局节流锁 1.1s/请求 + 并发 2
- 响应 `{success, code, data.items[{id, info_hash, name, size, download_url}]}`
- download_url 带 dl_token（1800s 有效）；下载 403 时用 `?apikey=` 直链重试一次
- **死种判定**: 通用通道靠 HTML 关键词；HDArea 不标死种（REST 接口）

## 5. 下载器规范

### 5.1 qBittorrent (qbittorrent-api)
- torrents_add 参数: `save_path, is_skip_checking, is_paused+is_stopped(同传，见源码注释), tags`
- 返回体三态: str "Ok."/"" → 成功；dict `success_count|pending_count` >0 → 成功；None → 成功；其他 → 失败
- get_all_info_hashes 失败 → 重连（auth_log_in）重试一次
- 状态名兼容: `pausedUP`(qB<5.x) / `stoppedUP`(qB≥5.x) / `completed`(TR)

### 5.2 Transmission
- add_torrent(torrent=bytes|url, download_dir, paused)；**无 skip_checking/tags 等价参数**（日志显式声明忽略）

### 5.3 通用
- `create_downloader` 缺 host 抛可读错误；type ∈ {qbittorrent(默认), transmission}
- 双下载器: destination 必须可连（失败 RuntimeError），source 失败仅告警降级

## 6. 统计口径（ReseedStats，GUI 仪表盘数据源）

```
total_torrents    扫描到的 .torrent 文件数
new_torrents      SQLite 中不存在的（pieces_hash）
duplicate_count   pieces_hash 重复数（total - unique）
tracker_skip_count 同站 tracker 域名已知的（阶段2前置排除）
matched_count     len(match_map)（至少一个站点命中的唯一 pieces_hash）
succeeded_count   成功添加到下载器
failed_count      阶段3 piece_failed 累计（仅全部站点都失败的种子）
dead_count        dead_cache.size()（含持久化载入的）
skipped_count     阶段3静默跳过（源种子离线/下载中/已知死种）
site_details      {site: 匹配数}
site_match_counts {site: 匹配数（match_map 展开）}
site_succeeded    {site: 成功数}
site_failed       {site: 失败数}
failed_sites      无匹配/查询异常的站点列表
credential_failures {site: 首个凭据类错误 reason}
```

四档状态灯: 🟢 succeeded>0 且 failed=0 ｜ 🟡 succeeded>0 ｜ 🔵 matched>0 ｜ ⚪ 无数据
排序: 成功率↓ → succeeded↓ → matched↓ → name↑（稳定排序）

## 7. SQLite Schema（rusqlite 需保持兼容，老库直接可用）

```sql
pieces_cache(pieces_hash TEXT PK, info_hash TEXT, file_name TEXT, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)
reseed_history(id INTEGER PK AUTOINCREMENT, pieces_hash TEXT, site_name TEXT, torrent_id INTEGER,
               created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
               UNIQUE(pieces_hash, site_name, torrent_id))  -- 首次升级去重后建
dead_torrents(site_name TEXT, torrent_id INTEGER, first_dead_at TIMESTAMP, last_dead_at TIMESTAMP,
              PRIMARY KEY(site_name, torrent_id))
-- 索引: idx_pieces_hash(pieces_hash), idx_dead_first(first_dead_at)
```
- `load_persistent_dead(min_days=15)`: julianday('now') - julianday(first_dead_at) >= 15
- 写入均走 `INSERT OR IGNORE` / upsert；批量查询按 500 一批 IN 查询

## 8. 调度器规范

- 触发: ① CLI `schedule` ② config scheduler.enabled=true 时 reseed 自动切换 ③ SEEDHOUND_MODE=schedule
- 每轮: cookiecloud.enabled 时先 sync_cookies → run_once（每轮重建引擎）
- 间隔: max(1.0, interval_minutes*60)；startup_delay 先等；run_on_start=false 先等一个完整间隔
- 优雅退出: 信号置 stop_event，当前轮完成后退出；等待期间收到信号立即退出
- 日志: 定时模式 keep_runs=0（不裁剪），一次性 keep_runs=3（按 RUN_MARKER="SeedHound 辅种引擎启动"分割）

## 9. 通知规范（飞书）

- 卡片: 总览(8指标) + hr + 站点表(markdown) + hr + note 页脚(耗时/站点/命中/无匹配/死种) + 无匹配列表 note(≤10 个后"等 N 个")
- 签名: timestamp(秒) + "\n" + secret 作为 HMAC-SHA256 **密钥**，对空消息摘要，Base64
- 响应 code==0 或 StatusCode==0 → 成功
- 凭据失效红色卡片: 每站点 `**{name}**：\`{reason}\`` + 提示 sync-cookies

## 10. 安全规范（必须继承）

- 日志脱敏过滤器（handler 层，不依赖调用点）: `passkey= / api[_-]?key= / dl_token= / X-API-Key:` 后的值 → `***`
- 配置不入库；代码零凭据默认值；测试不读真实配置
- CookieCloud 解密: md5("{uuid}-{password}") 前 16 字节为 key 材料，OpenSSL Salted__ + EVP_BytesToKey(MD5, 48字节) → AES-256-CBC

## 11. 测试基线（189 项 pytest）

- Golden 值（外部手写，Rust 测试必须复用同样值）:
  - `INFO_GOLDEN_BYTES = b"d6:lengthi1234e4:name8:test.dat12:piece lengthi65536e6:pieces20:" + b"\x00"*20 + b"e"`
  - `INFO_HASH_GOLDEN = "35471bc58ffcae4564c40faf40ff75dbbf97c36a"`（bencode(SAMPLE_INFO.info) 的 SHA1）
  - `PIECES_HASH_GOLDEN = "6768033e216468247bd031a0a2d9876d79818f8f"`（SHA1(b"\x00"*20)）
- bencode 行为: `i42e→42`, `i-7e→-7`, `li1ei2ei3ee→[1,2,3]`, `le→[]`, `d3:foo3:bare→{foo:bar}`, `4:spam→b"spam"`；编码时 dict 键按字典序
- 解析容错: 无 info / 无 pieces → None；announce_list 嵌套提取；files 求和 total_size

## 12. Python→Rust 依赖映射

| Python | Rust crate | 备注 |
|---|---|---|
| aiohttp | reqwest (rustls) | pool_max_idle_per_host(0) 等价 force_close |
| aiosqlite | rusqlite (bundled) | schema 完全兼容 |
| pycryptodome | aes + md-5 + base64 | CookieCloud EVP_BytesToKey |
| pyyaml | serde_yaml | 保留未知字段告警需用 Value 预校验 |
| qbittorrent-api | 手写 reqwest（本来就是薄封装） | 三态返回判定是关键坑 |
| transmission-rpc | 手写 reqwest | TR RPC 是 JSON-RPC over HTTP |
| argparse | clap 4 | 子命令 + SUPPRESS 语义 |
| hashlib/hmac | sha1, sha2, hmac | |
| xml.etree | quick-xml | Torznab 解析 |
| — | tokio (full), tracing, rayon, anyhow/thiserror, url | |
| — | GPUI (Zed), tray-icon, winit | GUI/托盘（见重写计划） |

## 13. 已知实现陷阱（重写时必须处理）

1. **bencode info 重编码**: info_hash 需要**字节级还原** info 字典再 SHA1。零拷贝解析时必须记录 info 区间的原始字节偏移（start/end），直接 slice 做 SHA1，避免重排序/重编码差异。
2. qB `is_paused=False` 折叠问题: 两个参数（is_paused+is_stopped）都要显式传。
3. `_ensure_started` 的 3×0.6s 轮询 + pausedUP/stoppedUP 双状态名。
4. HDArea dl_token 1800s 有效期: 查询与下载间隔过长 → 403 → apikey 直链重试。
5. 限流响应体必须同时匹配关键词+数字（或仅关键词→60s），不能只看状态码。
6. reseed_history 唯一索引首次升级需去重（老库兼容）。
7. Jackett dl_cache（JSON 文件 data/jackett_dl_cache.json）: url → pieces_hash 缓存，hash 不匹配直接跳过下载。
8. cookie_sync 写回 sites.yaml 用**文本行处理**（保留注释与顺序），不用 YAML dump。
9. Windows 控制台 UTF-8: Python 用 reconfigure；Rust GUI 模式无此问题，CLI 模式用 UTF-8 输出 API。
10. 双下载器语义: destination 连不上 = 整轮失败；source 连不上 = 降级扫描（torrent_dir 仍可来自 source 配置）。
