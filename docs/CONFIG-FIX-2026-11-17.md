# 配置读写与 qB 5.x 连接实机修复记录

日期：2026-11-17 · 状态：✅ 85 测试全通过 / clippy 0 警告 / 真实 config.yaml + sites.yaml（100 站）+ qB 5.3.0 实机验证通过

## 1. 现象

用户报告两件事，日志同时暴露第三、第四处缺陷：

| 现象 | 证据 |
|---|---|
| GUI 设置页看不到已配置的 qB 端口 | `deploy/config.yaml` 中 `port: ` 为空；GUI 显示空值 |
| 点「保存全部」报错 / 保存后引擎起不来 | `logs/reseed.log`: `config 重读失败(沿用旧配置): invalid type: integer '1357924', expected a string`；重启后 `配置加载失败，调度未启动: ...` |
| 引擎连不上下载器 | `[ERROR] 引擎启动失败: 下载器连接失败`（端口被清空后回退 8080；即便端口正确，qB 5.x 也会失败——见 §2.4） |

## 2. 根因（四处，读写路径对称性缺失）

### 2.1 数值标量读不出（GUI 显示空白 → 保存时清空）

`config_store.rs::load_downloader_form` 一律用 `Value::as_str()` 读字段。YAML 把 `port: 9091` 解析成**整数**，`as_str()` 返回 `None` → 表单显示空串；点保存时把空串写回磁盘，端口被抹掉。

同类隐患：`username` / `password` / `uuid` / `api_key` / `passkey` / `log.level` 等字段只要值全数字（如纯数字密码 `1357924`）就会显示为空并在保存时被清空。

### 2.2 写回不加引号（字符串被写成了别的类型）

`save_*` 直接把表单字符串拼进 YAML：`password: 1357924` 落盘后是**整数**，`Config::load` 反序列化 `Option<String>` 直接失败——这就是日志里的 `invalid type: integer '1357924', expected a string`。同理，含 `#`、`: `、首字符 `-`、Windows 反斜杠路径的值都可能写出歧义或非法 YAML。

### 2.3 站点补行缩进推断错误（保存直接报「YAML 无效」）

`upsert_site_field` 用 `indent > 2` 猜字段缩进，默认 4。Python 版导出的 sites.yaml 是**顶格** `- name:`，字段缩进 2 —— 给缺 `enabled` 的站点补行会写成 4 空格：

```yaml
- name: example
  url: https://pt.soulvoice.club
    enabled: true      # ← 非法缩进，整份文件解析失败
```

`ensure_valid_yaml` 拒写 → 用户看到「保存失败」。用户 100 站里有 62 站没有 `enabled` 键，必踩。

### 2.4 qB 5.x 会话 cookie 改名（下载器永远连不上）

`downloader.rs::qb_login` 只从 `Set-Cookie` 里找 `SID=`。实测 qB 5.3.0 登录成功返回 **204 + `QBT_SID_9091=<值>`**，没有 `SID`，于是 `sid = None` → `EngineError::DownloaderConnect`。

存量代码另有一处不规范：把会话值当 **`SID` 请求头**发送，而 HTTP 语义要求放 `Cookie: 名=值`。

## 3. 修复

| 项 | 位置 | 做法 |
|---|---|---|
| 标量读取 | `config_store.rs` | 新增 `scalar_text()`：String / Number / Bool 统一转字符串，所有 `load_*_form` 改用它 |
| 标量写回 | `config_store.rs` | 新增 `yaml_scalar()`：空串→`""`；数字/布尔/null 字面量、含 `#`/`: `/反斜杠/控制字符/首尾空白→双引号转义；其余直出 |
| 数字字段 | `config_store.rs` | 端口空值→`defaults::DL_PORT`，越界拒写（70000 报错且不动磁盘）；调度间隔 / 并发 / 批量 / Jackett 并发与超时同样校验后写 |
| 产物校验 | `config_store.rs` | `ensure_valid_config()` / `ensure_valid_sites()`：语法 + **强类型**双重校验（`ConfigFile` / `SitesFile`），类型不合法直接拒写并回报字段级错误 |
| 块内写回 | `config_store.rs` | `upsert_section_field` 只认一级缩进（避免覆盖 `downloader.source.port`），行内 `{}`/`null` 先展开，补行插到同级字段之后 |
| 站点补行 | `config_store.rs` | `upsert_site_field` 以条目首个子行实际缩进为准（顶格 `- name:` → 字段 2 空格），新站点名/URL/passkey 同样走 `yaml_scalar` |
| 常量集中 | `sh-core/config.rs` | 新增 `defaults::DL_PORT = 8080`、`DL_TYPE`、`DL_USERNAME`，`downloader_conn` 与 GUI 表单共用 |
| qB 会话 | `sh-engine/downloader.rs` | 新增 `parse_qb_session()`：同时认 `SID`（≤4.x）与 `QBT_SID_*`（5.x）；会话改存 `(名, 值)`，请求头改发 `Cookie: 名=值` |

## 4. 实机验证（用户真实配置，100 站 / 41890 种子）

环境：本机 `192.168.10.111`，qBittorrent **5.3.0 portable**，WebUI `0.0.0.0:9091`，`LocalHostAuth=false`。
探测事实：`127.0.0.1:9091/api/v2/app/version` → `200 v5.3.0`（本机免认证）；`192.168.10.111:9091` → `403`（`AuthSubnetWhitelist` 只含 192.168.1.0/24）；错误口令从非本机来源 → `401`。

| 步骤 | 结果 |
|---|---|
| 真实 config.yaml + sites.yaml 跑完整保存链路（下载器/站点/调度/全局/飞书/CookieCloud/Jackett/日志） | 全链路无报错 |
| 保存后强类型重载 | `生效 port=9091`、`sites=65`（100 站准入后）、`mode=pieces_hash` |
| 数据保全 | 站点 100 → 100，`cookie:` 行 88 → 88，`download_interval` 等未管字段原样保留 |
| 引擎端到端（`--dry-run --site xiudaoyuan`） | 扫描 41890 个种子 → 去重后待查 4244 → 阶段2 站点查询正常，**无「下载器连接失败」**（qB 5.3.0 登录通过） |

## 5. 现场配置修正与待办

- `deploy/config.yaml` 原 `port:` / `password:` 已被旧版本 GUI 写空；端口经探测恢复为 **9091**。
- `host` 由 `192.168.10.111` 改为 `127.0.0.1`：qB 的 `AuthSubnetWhitelist` 不含本网段，只能走「本机免认证」；若要从其它机器连接，需在 qB「选项 → Web UI」把本网段加入白名单或设置口令，再把 host 改回局域网地址。
- `torrent_dir` 指向运行中实例 `qBittorrent_5.3.0_portable\...\BT_backup`（原指向 4.6.7 的历史备份，41890 vs 41005 个种子）。
- **被抹掉的口令无法从 qB 的 PBKDF2 哈希恢复**：GUI 里重新填写，或继续使用本机免认证。
- CookieCloud 仍报 `PKCS7 填充非法`：配置里的 `password` 与 CookieCloud 服务端不一致（配置级问题，非代码缺陷）。
- 本机无 Windows SDK（缺 `fxc.exe`），`cargo build --release` 无法完成；本次 GUI 验证使用 debug 产物（`deploy/gui-check/`）。release 打包需在装有 SDK 的机器或安装 Windows SDK 后执行。

## 6. 第二轮（同日 21:40）：CookieCloud 解密算法 + GUI 卡死/abort

### 6.1 现象

- 在 GUI 里改 CookieCloud 配置后应用卡死，随后进程 abort（WER `0xc0000409` / `FAST_FAIL_FATAL_APP_EXIT`）。
- 日志固定报 `CookieCloud 获取/解密失败: PKCS7 填充非法`，但同一组 uuid/口令在别处可用。

### 6.2 根因

1. **口令派生错**：旧实现把 `md5("{uuid}-{password}")` 的**原始 16 字节**当作 EVP 口令。
   CookieCloud 官方算法是取该 MD5 **十六进制串的前 16 个字符**（ASCII）当 CryptoJS 口令，
   再由 EVP_BytesToKey(MD5, 口令, salt) 派生 key/iv 做 AES-256-CBC。
   实测验证：对本机服务端 1 MB 密文用 `openssl enc -d -aes-256-cbc -md md5 -pass pass:<按上述规则算出的口令>` 可直接解出 JSON（cmd 环境同时验证了派生规则）。
2. **未支持新格式**：0.3.0 起服务端返回 `crypto_type: aes-128-cbc-fixed`（key=口令 16 字节、IV 全 0、无 `Salted__` 头），旧实现只看 `Salted__`。
3. **退出阻塞**：`on_app_quit` 在 UI 线程 `join()` 调度线程；引擎正在跑整轮（4 万种子扫描 + 站点查询）时关窗 → 窗口无响应，只能强杀，表现为「卡死 + abort」。

### 6.3 修复

| 项 | 位置 | 做法 |
|---|---|---|
| 口令派生 | `cookiecloud.rs` | `passphrase = md5_hex(uuid-password)[..16]`，EVP 循环改用该口令 |
| 双算法 | `cookiecloud.rs` | legacy（Salted__ + AES-256-CBC）与 `aes-128-cbc-fixed`（AES-128-CBC + 零 IV）；读 `crypto_type`，缺省按密文头判断 |
| 解填充 | `cookiecloud.rs` | 改用 `decrypt_padded_mut::<Pkcs7>()`（顺带修掉旧测试里「每块新建 cipher 导致 IV 不串链」的错误） |
| 退出 | `main.rs` | 后台线程 join + `recv_timeout(2s)`，超时告警后继续退出 |
| panic 自诊断 | `main.rs` | panic 钩子把位置/线程/原因追加到 `logs/panic.log`（独立文件，避免与 tracing 锁互锁） |
| 状态锁 | `main.rs` | `lock().unwrap()` → `if let Ok`，锁中毒不再连锁 panic |
| 遮罩输入 | `text_input.rs` | 每个字符一个圆点（旧实现固定 12 个，长口令被截断），光标/选区按字符投影到遮罩串 |

### 6.4 验证

- 真实服务端（本机 8082，1 MB 密文）：`sync-cookies` **同步完成 86/100 站点**。
- GUI 实机：启动即同步成功并「已重新加载 sites 配置」；关闭窗口 `EXIT=0`，无 panic 记录（旧实现会卡在 join）。
- 测试 88 全通过（新增 passphrase 夹值、legacy/fixed 往返、错误口令拒绝、遮罩投影），clippy 0 警告。
