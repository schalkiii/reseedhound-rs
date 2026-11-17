//! 下载器适配层：qBittorrent / Transmission REST API。
//!
//! 对齐 Python src/engine/downloader.py：
//! - qB：login → torrents/add（skip_checking=1 等价 skip_hash_check）→ torrents/info 状态确认
//! - TR：rpc-version 探测 → session-get → torrent-add（paused:true 等价）
//! - 状态语义：qB pausedUP / TR stoppedUP = 做种完成
//!
//! Python 依赖 qbittorrent-api / transmission_rpc 库；Rust 版手写 REST（这两个
//! 库本身也是 REST 封装，Python 版 client.py 同样包了一层）。

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::error::EngineError;

#[derive(Debug, Clone, PartialEq)]
pub enum DownloaderKind {
    Qbittorrent,
    Transmission,
}

impl DownloaderKind {
    pub fn from_type(s: &str) -> Result<Self, EngineError> {
        match s.to_lowercase().as_str() {
            "qbittorrent" | "qb" => Ok(DownloaderKind::Qbittorrent),
            "transmission" | "tr" => Ok(DownloaderKind::Transmission),
            other => Err(EngineError::UnsupportedDownloaderType(other.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloaderConn {
    pub kind: DownloaderKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

pub struct DownloaderClient {
    conn: DownloaderConn,
    http: reqwest::Client,
    /// qB 会话 cookie（名, 值）：qB ≤4.x 为 `SID`，5.x 改名为 `QBT_SID_<端口>`
    qb_session: Option<(String, String)>,
}

/// 下载结果：添加的任务 hash + 初始状态。
#[derive(Debug, Clone)]
pub struct AddResult {
    pub info_hashes: Vec<String>,
}

/// 从 Set-Cookie 头提取 qB 会话 cookie。
///
/// qB ≤4.x 用 `SID=<值>`，5.x 起改名为 `QBT_SID_<WebUI 端口>=<值>`；
/// 只认 `SID` 会让 5.x 登录成功（204）却被判成连接失败。
fn parse_qb_session<'a>(set_cookies: impl Iterator<Item = &'a str>) -> Option<(String, String)> {
    set_cookies
        .filter_map(|line| line.split(';').next())
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "SID" || name.starts_with("QBT_SID_"))
        .map(|(name, value)| (name.to_string(), value.to_string()))
}

impl DownloaderClient {
    pub fn new(conn: DownloaderConn) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self {
            conn,
            http,
            qb_session: None,
        }
    }

    pub fn conn_kind(&self) -> &DownloaderKind {
        &self.conn.kind
    }

    /// TR 轻量连通性探测（409 + session id 即可达）。
    pub async fn tr_ping(&self) -> bool {
        self.tr_session_id().await.is_ok()
    }

    /// qB 拉全量种子 info_hash（ torrents/info 全表）。
    pub async fn qb_get_all_info_hashes(&self) -> Result<HashSet<String>, EngineError> {
        let url = format!("{}/api/v2/torrents/info", self.base_url());
        let mut req = self.http.get(&url);
        if let Some((name, value)) = self.qb_auth() {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        let arr: Value = resp
            .json()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        let mut set = HashSet::new();
        if let Some(items) = arr.as_array() {
            for item in items {
                if let Some(h) = item.get("hash").and_then(|v| v.as_str()) {
                    set.insert(h.to_lowercase());
                }
            }
        }
        Ok(set)
    }

    /// 以字节流添加种子（qB multipart / TR metainfo base64），带 info_hash 幂等检查。
    /// save_path：辅种种子落盘目录（源种子 save_path；None = 下载器默认目录）。
    pub async fn add_torrent_bytes(
        &mut self,
        torrent_bytes: &[u8],
        skip_hash_check: bool,
        auto_start: bool,
        tag: Option<&str>,
        info_hash: &str,
        save_path: Option<&str>,
    ) -> bool {
        match self.conn.kind {
            DownloaderKind::Qbittorrent => {
                // 幂等：已存在则视为成功
                if let Some(state) = self.qb_torrent_status(info_hash).await {
                    if state == "pausedUP" || state == "stoppedUP" || state == "uploading" {
                        return true;
                    }
                }
                if self.qb_session.is_none() && self.qb_login().await.is_err() {
                    return false;
                }
                let url = format!("{}/api/v2/torrents/add", self.base_url());
                let mut form = reqwest::multipart::Form::new().part(
                    "torrents",
                    reqwest::multipart::Part::bytes(torrent_bytes.to_vec())
                        .file_name("reseed.torrent"),
                );
                if skip_hash_check {
                    form = form.text("skip_checking", "true");
                }
                if !auto_start {
                    form = form.text("paused", "true");
                }
                if let Some(tag) = tag {
                    if !tag.is_empty() {
                        form = form.text("tags", tag.to_string());
                    }
                }
                if let Some(path) = save_path {
                    if !path.is_empty() {
                        form = form.text("savepath", path.to_string());
                    }
                }
                let mut req = self.http.post(&url).multipart(form);
                if let Some((name, value)) = self.qb_auth() {
                    req = req.header(name, value);
                }
                match req.send().await {
                    Ok(r) => r.status().is_success(),
                    Err(_) => false,
                }
            }
            DownloaderKind::Transmission => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(torrent_bytes);
                let body = json!({
                    "method": "torrent-add",
                    "arguments": {
                        "metainfo": b64,
                        "paused": !auto_start,
                    }
                });
                let url = format!("{}/transmission/rpc", self.base_url());
                let Ok(session_id) = self.tr_session_id().await else {
                    return false;
                };
                match self
                    .http
                    .post(&url)
                    .header("X-Transmission-Session-Id", &session_id)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(r) => match r.json::<Value>().await {
                        Ok(v) => v.get("result").and_then(|r| r.as_str()) == Some("success"),
                        Err(_) => false,
                    },
                    Err(_) => false,
                }
            }
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.conn.host, self.conn.port)
    }

    /// qB 登录（SID cookie）。
    pub async fn qb_login(&mut self) -> Result<(), EngineError> {
        let url = format!("{}/api/v2/auth/login", self.base_url());
        let params = [
            ("username", self.conn.username.as_str()),
            ("password", self.conn.password.as_str()),
        ];
        let resp = self
            .http
            .post(&url)
            .form(&params)
            .send()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        if !resp.status().is_success() {
            return Err(EngineError::DownloaderConnect);
        }
        // 会话 cookie 在 Set-Cookie：qB ≤4.x 是 `SID`，5.x 起是 `QBT_SID_<端口>`
        let session = parse_qb_session(
            resp.headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );
        match session {
            Some(s) => {
                self.qb_session = Some(s);
                Ok(())
            }
            None => Err(EngineError::DownloaderConnect),
        }
    }

    /// qB 鉴权头：`Cookie: <会话名>=<值>`（4.x / 5.x 通用写法）。
    fn qb_auth(&self) -> Option<(reqwest::header::HeaderName, reqwest::header::HeaderValue)> {
        let (name, value) = self.qb_session.as_ref()?;
        let header = reqwest::header::HeaderValue::from_str(&format!("{name}={value}")).ok()?;
        Some((reqwest::header::COOKIE, header))
    }

    /// qB 添加种子：文件路径形式（skip_checking 即 Python skip_hash_check / seedMode）。
    pub async fn qb_add_torrent_file(
        &mut self,
        torrent_path: &str,
        skip_hash_check: bool,
        auto_start: bool,
        tag: Option<&str>,
    ) -> Result<AddResult, EngineError> {
        if self.qb_session.is_none() {
            self.qb_login().await?;
        }
        let url = format!("{}/api/v2/torrents/add", self.base_url());
        let file_bytes = match std::fs::read(torrent_path) {
            Ok(b) => b,
            Err(_) => return Err(EngineError::DownloaderConnect),
        };

        let mut form = reqwest::multipart::Form::new().part(
            "torrents",
            reqwest::multipart::Part::bytes(file_bytes).file_name(
                std::path::Path::new(torrent_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "t.torrent".into()),
            ),
        );
        if skip_hash_check {
            form = form.text("skip_checking", "true");
        }
        if !auto_start {
            form = form.text("paused", "true");
        }
        if let Some(tag) = tag {
            if !tag.is_empty() {
                form = form.text("tags", tag.to_string());
            }
        }

        let mut req = self.http.post(&url).multipart(form);
        if let Some((name, value)) = self.qb_auth() {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        if !resp.status().is_success() {
            return Err(EngineError::DownloaderConnect);
        }
        // qB add 接口返回 "Ok."，无 hash；用解析 info_hash 由调用方提供
        Ok(AddResult {
            info_hashes: vec![],
        })
    }

    /// qB 查询种子状态（pausedUP = 做种完成）。
    pub async fn qb_torrent_status(&self, info_hash: &str) -> Option<String> {
        let url = format!("{}/api/v2/torrents/info", self.base_url());
        let mut req = self.http.get(&url).query(&[("hashes", info_hash)]);
        if let Some((name, value)) = self.qb_auth() {
            req = req.header(name, value);
        }
        let resp = req.send().await.ok()?;
        let arr: Value = resp.json().await.ok()?;
        arr.as_array()?
            .first()?
            .get("state")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// qB 查询种子保存路径与状态（源种子状态前置检查 + save_path 传递用）。
    /// 对齐 Python get_torrent_info：返回 (save_path, state)，种子不存在返回 None。
    pub async fn qb_torrent_info(&self, info_hash: &str) -> Option<(String, String)> {
        let url = format!("{}/api/v2/torrents/info", self.base_url());
        let mut req = self.http.get(&url).query(&[("hashes", info_hash)]);
        if let Some((name, value)) = self.qb_auth() {
            req = req.header(name, value);
        }
        let resp = req.send().await.ok()?;
        let arr: Value = resp.json().await.ok()?;
        let t = arr.as_array()?.first()?;
        let save_path = t
            .get("save_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state = t.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Some((save_path, state))
    }

    /// TR：torrent-add（paused = !auto_start）。
    pub async fn tr_add_torrent_file(
        &self,
        torrent_path: &str,
        auto_start: bool,
        _tag: Option<&str>,
    ) -> Result<AddResult, EngineError> {
        let file_bytes = std::fs::read(torrent_path).map_err(|_| EngineError::DownloaderConnect)?;
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(file_bytes)
        };
        let body = json!({
            "method": "torrent-add",
            "arguments": {
                "metainfo": b64,
                "paused": !auto_start,
            }
        });
        let url = format!("{}/transmission/rpc", self.base_url());
        // TR 需要 X-Transmission-Session-Id：409 响应头里拿
        let session_id = self.tr_session_id().await?;
        let resp = self
            .http
            .post(&url)
            .header("X-Transmission-Session-Id", &session_id)
            .json(&body)
            .send()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        let v: Value = resp
            .json()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        if v.get("result").and_then(|r| r.as_str()) == Some("success") {
            Ok(AddResult {
                info_hashes: vec![],
            })
        } else {
            Err(EngineError::DownloaderConnect)
        }
    }

    async fn tr_session_id(&self) -> Result<String, EngineError> {
        let url = format!("{}/transmission/rpc", self.base_url());
        let resp = self
            .http
            .post(&url)
            .body("{}")
            .send()
            .await
            .map_err(|_| EngineError::DownloaderConnect)?;
        if let Some(sid) = resp
            .headers()
            .get("X-Transmission-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            return Ok(sid.to_string());
        }
        Err(EngineError::DownloaderConnect)
    }

    /// 统一入口：按类型分派。
    pub async fn add_torrent_file(
        &mut self,
        torrent_path: &str,
        skip_hash_check: bool,
        auto_start: bool,
        tag: Option<&str>,
    ) -> Result<AddResult, EngineError> {
        match self.conn.kind {
            DownloaderKind::Qbittorrent => {
                self.qb_add_torrent_file(torrent_path, skip_hash_check, auto_start, tag)
                    .await
            }
            DownloaderKind::Transmission => {
                self.tr_add_torrent_file(torrent_path, auto_start, tag)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_type() {
        assert_eq!(
            DownloaderKind::from_type("qbittorrent").unwrap(),
            DownloaderKind::Qbittorrent
        );
        assert_eq!(
            DownloaderKind::from_type("QBitTorrent").unwrap(),
            DownloaderKind::Qbittorrent
        );
        assert_eq!(
            DownloaderKind::from_type("transmission").unwrap(),
            DownloaderKind::Transmission
        );
        assert!(DownloaderKind::from_type("aria2").is_err());
    }

    #[test]
    fn qb_session_cookie_v4_and_v5() {
        // qB ≤4.x：SID=<值>
        let v4 = parse_qb_session(["SID=abc123; HttpOnly; path=/", "other=1"].into_iter());
        assert_eq!(v4, Some(("SID".to_string(), "abc123".to_string())));

        // qB 5.x：QBT_SID_<端口>=<值>（实测 qB 5.3.0 登录返回 204 + 该 cookie）
        let v5 = parse_qb_session(
            ["QBT_SID_9091=C9xAiq+7x4A; HttpOnly; expires=Tue, 17-Nov-2026 14:00:24 GMT; path=/"]
                .into_iter(),
        );
        assert_eq!(
            v5,
            Some(("QBT_SID_9091".to_string(), "C9xAiq+7x4A".to_string()))
        );

        // 无会话 cookie（登录失败）→ None
        assert_eq!(parse_qb_session(["foo=bar"].into_iter()), None);
    }
}
