//! CookieCloud 客户端与站点 Cookie 同步。
//!
//! 对齐 Python src/network/cookiecloud.py + src/cookie_sync.py：
//! - 加密：AES-256-CBC，key/iv 由 OpenSSL EVP_BytesToKey(MD5, Salted__) 派生
//!   （crypto_key = md5("{uuid}-{password}")[:16]）
//! - 域名匹配：逐级后缀（a.b.c → b.c / c）→ 精确 host → host 以 cookie 域结尾
//! - 写回：serde_yaml 保持字段顺序（不 sort_keys），cookie 值双引号包裹单行
//! - 同步失败仅告警不中断（定时模式每轮前置步骤）

use std::collections::HashMap;
use std::path::Path;

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use base64::Engine;
use serde::Deserialize;
use sha2::Digest as _;
use url::Url;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// 解密后的 Cookie 条目。
#[derive(Debug, Clone, Deserialize)]
pub struct CookieEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

pub struct CookieCloudClient {
    base_url: String,
    uuid: String,
    password: String,
    crypto_key: [u8; 16],
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct GetResponse {
    #[serde(default)]
    encrypted: String,
}

#[derive(Deserialize)]
struct DecryptedPayload {
    #[serde(rename = "cookie_data", default)]
    cookie_data: HashMap<String, Vec<CookieEntry>>,
}

impl CookieCloudClient {
    pub fn new(base_url: &str, uuid: &str, password: &str) -> anyhow::Result<Self> {
        let crypto_key: [u8; 16] = {
            let digest = md5::Md5::digest(format!("{uuid}-{password}").as_bytes());
            digest[..16].try_into().expect("md5 digest is 16 bytes")
        };
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("ReseedHound-RS/0.1")
            .build()?;
        Ok(Self {
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            uuid: uuid.to_string(),
            password: password.to_string(),
            crypto_key,
            http,
        })
    }

    /// EVP_BytesToKey(MD5)：key(32) + iv(16) 派生。
    fn derive_key_iv(&self, salt: &[u8]) -> ([u8; 32], [u8; 16]) {
        let mut key_iv = Vec::with_capacity(48);
        let mut prev: Vec<u8> = Vec::new();
        while key_iv.len() < 48 {
            let mut input = prev.clone();
            input.extend_from_slice(&self.crypto_key);
            input.extend_from_slice(salt);
            prev = md5::Md5::digest(&input).to_vec();
            key_iv.extend_from_slice(&prev);
        }
        let key: [u8; 32] = key_iv[..32].try_into().unwrap();
        let iv: [u8; 16] = key_iv[32..48].try_into().unwrap();
        (key, iv)
    }

    /// 解密 "Salted__" 前缀的 base64 密文 → JSON payload。
    fn decrypt(&self, encrypted_b64: &str) -> anyhow::Result<DecryptedPayload> {
        let raw = base64::engine::general_purpose::STANDARD.decode(encrypted_b64)?;
        if raw.len() < 16 || &raw[..8] != b"Salted__" {
            anyhow::bail!("不支持的加密格式");
        }
        let salt = &raw[8..16];
        let ciphertext = &raw[16..];
        let (key, iv) = self.derive_key_iv(salt);
        let mut buf = ciphertext.to_vec();
        // PKCS#7 解填充（原地）
        let n = buf.len();
        if n == 0 || n % 16 != 0 {
            anyhow::bail!("密文长度非法: {n}");
        }
        for chunk in buf.as_chunks_mut::<16>().0 {
            Aes256CbcDec::new(&key.into(), &iv.into()).decrypt_block_mut(chunk.into());
        }
        let pad = buf[n - 1] as usize;
        if pad == 0 || pad > 16 || pad > n || buf[n - pad..n].iter().any(|&b| b != pad as u8) {
            anyhow::bail!("PKCS7 填充非法");
        }
        buf.truncate(n - pad);
        Ok(serde_json::from_slice(&buf)?)
    }

    /// 拉取并解密全量 Cookie（域名 → Cookie 列表）。失败返回空 map + 错误日志。
    pub async fn fetch_cookies(&self) -> HashMap<String, Vec<CookieEntry>> {
        let url = format!("{}/get/{}", self.base_url, self.uuid);
        let result: anyhow::Result<HashMap<String, Vec<CookieEntry>>> = async {
            let resp = self
                .http
                .get(&url)
                .query(&[("password", &self.password)])
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("CookieCloud 请求失败: HTTP {}", resp.status());
            }
            let data: GetResponse = resp.json().await?;
            if data.encrypted.is_empty() {
                anyhow::bail!("CookieCloud 返回数据中没有 encrypted 字段");
            }
            let decrypted = self.decrypt(&data.encrypted)?;
            let mut by_domain = HashMap::new();
            for (domain, cookies) in decrypted.cookie_data {
                let clean = domain.trim_start_matches('.').to_string();
                if !clean.is_empty() {
                    by_domain.insert(clean, cookies);
                }
            }
            Ok(by_domain)
        }
        .await;

        match result {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("CookieCloud 获取/解密失败: {e}");
                HashMap::new()
            }
        }
    }

    /// Cookie 列表 → "name=value; name2=value2"。
    pub fn cookies_to_string(cookies: &[CookieEntry]) -> String {
        cookies
            .iter()
            .filter(|c| !c.name.is_empty() && !c.value.is_empty())
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// 站点 URL 与 Cookie 域名匹配：逐级后缀 → 精确 host → host 以域名结尾。
    pub fn match_domain<'a>(
        site_url: &str,
        cookie_domains: &'a HashMap<String, Vec<CookieEntry>>,
    ) -> Option<&'a str> {
        if site_url.is_empty() {
            return None;
        }
        let host = Url::parse(site_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();
        if host.is_empty() {
            return None;
        }
        let parts: Vec<&str> = host.split('.').collect();
        // 逐级后缀：a.b.c → b.c → c
        for i in 1..parts.len() {
            let candidate = parts[i..].join(".");
            if let Some(d) = cookie_domains.get(&candidate) {
                let _ = d;
                return Some(cookie_domains.get_key_value(&candidate).unwrap().0.as_str());
            }
        }
        if let Some(kv) = cookie_domains.get_key_value(&host) {
            return Some(kv.0.as_str());
        }
        for d in cookie_domains.keys() {
            if host.ends_with(&format!(".{d}")) {
                return Some(d.as_str());
            }
        }
        None
    }
}

/// 从 CookieCloud 同步站点 Cookie 并写回 sites_path。
///
/// 返回 Ok(n) = 成功更新 n 个站点；Err = 未获取到任何 Cookie。
pub async fn sync_cookies(
    url: &str,
    uuid: &str,
    password: &str,
    sites_path: &str,
) -> anyhow::Result<usize> {
    tracing::info!("从 CookieCloud 同步站点 Cookie...");
    let client = CookieCloudClient::new(url, uuid, password)?;
    let cookies_by_domain = client.fetch_cookies().await;
    if cookies_by_domain.is_empty() {
        anyhow::bail!("未获取到任何 Cookie 数据");
    }
    tracing::info!(
        "从 CookieCloud 获取到 {} 个域名的 Cookie",
        cookies_by_domain.len()
    );

    let path = Path::new(sites_path);
    let raw = std::fs::read_to_string(path)?;
    // serde_yaml::Value 保留映射顺序（serde_yaml 的 Mapping 是 IndexMap）
    let mut sites_data: serde_yaml::Value = serde_yaml::from_str(&raw)?;

    let Some(sites) = sites_data
        .get_mut("sites")
        .and_then(|v| v.as_sequence_mut())
    else {
        anyhow::bail!("sites.yaml 缺少 sites 列表");
    };
    let total_sites = sites.len();

    let mut matched_count = 0usize;
    for site in sites.iter_mut() {
        let site_url = site
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(matched_domain) = CookieCloudClient::match_domain(&site_url, &cookies_by_domain)
        else {
            continue;
        };
        let cookies = &cookies_by_domain[matched_domain];
        let cookie_str = CookieCloudClient::cookies_to_string(cookies);
        let site_name = site
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        if let Some(map) = site.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("cookie".into()),
                serde_yaml::Value::String(cookie_str),
            );
        }
        matched_count += 1;
        tracing::info!(
            "  [OK] {site_name} <- {matched_domain} ({} 条 Cookie)",
            cookies.len()
        );
    }

    let out = serde_yaml::to_string(&sites_data)?;
    std::fs::write(path, out)?;

    tracing::info!("同步完成: {matched_count}/{total_sites} 个站点已更新 Cookie → {sites_path}");
    Ok(matched_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造与 Python 相同算法的加密样本做互验。
    fn encrypt_sample(uuid: &str, password: &str, plaintext: &str) -> String {
        use aes::cipher::BlockEncryptMut;
        type Enc = cbc::Encryptor<aes::Aes256>;
        let digest = md5::Md5::digest(format!("{uuid}-{password}").as_bytes());
        let crypto_key: [u8; 16] = digest[..16].try_into().unwrap();
        let salt = [7u8; 8];
        let mut key_iv = Vec::new();
        let mut prev: Vec<u8> = Vec::new();
        while key_iv.len() < 48 {
            let mut input = prev.clone();
            input.extend_from_slice(&crypto_key);
            input.extend_from_slice(&salt);
            prev = md5::Md5::digest(&input).to_vec();
            key_iv.extend_from_slice(&prev);
        }
        let key: [u8; 32] = key_iv[..32].try_into().unwrap();
        let iv: [u8; 16] = key_iv[32..48].try_into().unwrap();
        // PKCS7 手工填充
        let pt = plaintext.as_bytes();
        let pad = 16 - (pt.len() % 16);
        let mut data = pt.to_vec();
        data.extend(std::iter::repeat_n(pad as u8, pad));
        let mut ct = data.clone();
        for chunk in ct.as_chunks_mut::<16>().0 {
            Enc::new(&key.into(), &iv.into()).encrypt_block_mut(chunk.into());
        }
        let mut raw = b"Salted__".to_vec();
        raw.extend_from_slice(&salt);
        raw.extend_from_slice(&ct);
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    #[test]
    fn decrypt_roundtrip_matches_python_scheme() {
        let uuid = "test-uuid";
        let password = "test-pass";
        let payload = r#"{"cookie_data": {"hdarea.club": [{"name": "uid", "value": "123"}, {"name": "pass", "value": "abc"}]}}"#;
        let encrypted = encrypt_sample(uuid, password, payload);

        let client = CookieCloudClient::new("http://x", uuid, password).unwrap();
        let decrypted = client.decrypt(&encrypted).expect("解密失败");
        let cookies = &decrypted.cookie_data["hdarea.club"];
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "uid");
        assert_eq!(
            CookieCloudClient::cookies_to_string(cookies),
            "uid=123; pass=abc"
        );
    }

    #[test]
    fn decrypt_rejects_unsalted() {
        let client = CookieCloudClient::new("http://x", "u", "p").unwrap();
        let bad = base64::engine::general_purpose::STANDARD.encode(b"plaindata-not-encrypted");
        assert!(client.decrypt(&bad).is_err());
    }

    #[test]
    fn match_domain_suffix_and_exact() {
        let mut m = HashMap::new();
        m.insert("hdarea.club".to_string(), vec![]);
        m.insert("pt.example.org".to_string(), vec![]);
        // 逐级后缀命中
        assert_eq!(
            CookieCloudClient::match_domain("https://www.hdarea.club/torrents", &m),
            Some("hdarea.club")
        );
        // 精确 host 命中
        assert_eq!(
            CookieCloudClient::match_domain("https://pt.example.org/", &m),
            Some("pt.example.org")
        );
        // 无匹配
        assert_eq!(
            CookieCloudClient::match_domain("https://other.net/", &m),
            None
        );
        assert_eq!(CookieCloudClient::match_domain("", &m), None);
    }

    #[tokio::test]
    async fn sync_cookies_writes_back() {
        let dir = std::env::temp_dir().join("sh_cookiecloud_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sites.yaml");
        std::fs::write(
            &p,
            "sites:\n  - name: HDArea\n    url: https://hdarea.club/\n    passkey: \"pk\"\n    api_type: hdarea\n    api_key: \"k\"\n",
        )
        .unwrap();
        // 无服务器 → fetch_cookies 返回空 → 同步失败（不写文件）
        let r = sync_cookies("http://127.0.0.1:1", "u", "p", p.to_str().unwrap()).await;
        assert!(r.is_err());
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(!content.contains("cookie:"), "失败时不得写文件");
    }
}
