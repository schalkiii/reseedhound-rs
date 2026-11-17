//! CookieCloud 客户端与站点 Cookie 同步。
//!
//! 对齐 CookieCloud 官方算法（README「Encryption and Decryption Algorithm」）：
//! - 口令 = `md5("{uuid}-{password}")` 十六进制**前 16 个字符**（不是 MD5 原始字节）
//! - legacy（默认）：CryptoJS 口令模式 → OpenSSL `Salted__` 包装，
//!   key/iv 由 EVP_BytesToKey(MD5, 口令, salt) 派生，AES-256-CBC + PKCS7
//! - aes-128-cbc-fixed（CookieCloud ≥0.3.0）：AES-128-CBC，key = 口令的 16 个 UTF-8 字节，
//!   IV 全 0，密文为纯 Base64（无 `Salted__` 头）
//! - 域名匹配：逐级后缀（a.b.c → b.c / c）→ 精确 host → host 以 cookie 域结尾
//! - 写回：serde_yaml 保持字段顺序（不 sort_keys），cookie 值双引号包裹单行
//! - 同步失败仅告警不中断（定时模式每轮前置步骤）

use std::collections::HashMap;
use std::path::Path;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use base64::Engine;
use serde::Deserialize;
use sha2::Digest as _;
use url::Url;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

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
    /// CryptoJS 口令：`md5("{uuid}-{password}")` 十六进制前 16 位（ASCII，16 字节）
    passphrase: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct GetResponse {
    #[serde(default)]
    encrypted: String,
    /// `legacy`（旧默认）或 `aes-128-cbc-fixed`（0.3.0 起）；缺省按密文头判断
    #[serde(default)]
    crypto_type: String,
}

#[derive(Deserialize)]
struct DecryptedPayload {
    #[serde(rename = "cookie_data", default)]
    cookie_data: HashMap<String, Vec<CookieEntry>>,
}

impl CookieCloudClient {
    pub fn new(base_url: &str, uuid: &str, password: &str) -> anyhow::Result<Self> {
        let passphrase = {
            let digest = md5::Md5::digest(format!("{uuid}-{password}").as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            hex[..16].to_string()
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
            passphrase,
            http,
        })
    }

    /// EVP_BytesToKey(MD5)：key(32) + iv(16) 派生（CryptoJS 口令模式同算法）。
    fn derive_key_iv(&self, salt: &[u8]) -> ([u8; 32], [u8; 16]) {
        let passphrase = self.passphrase.as_bytes();
        let mut key_iv = Vec::with_capacity(48);
        let mut prev: Vec<u8> = Vec::new();
        while key_iv.len() < 48 {
            let mut input = prev.clone();
            input.extend_from_slice(passphrase);
            input.extend_from_slice(salt);
            prev = md5::Md5::digest(&input).to_vec();
            key_iv.extend_from_slice(&prev);
        }
        let key: [u8; 32] = key_iv[..32].try_into().unwrap();
        let iv: [u8; 16] = key_iv[32..48].try_into().unwrap();
        (key, iv)
    }

    /// legacy：`Salted__` + 8 字节 salt + AES-256-CBC 密文（CryptoJS 口令模式）。
    fn decrypt_legacy(&self, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
        if raw.len() < 32 || &raw[..8] != b"Salted__" {
            anyhow::bail!("legacy 密文缺少 Salted__ 头或长度不足");
        }
        let (key, iv) = self.derive_key_iv(&raw[8..16]);
        let mut buf = raw[16..].to_vec();
        let plain = Aes256CbcDec::new(&key.into(), &iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|_| anyhow::anyhow!("PKCS7 填充非法（口令错误或密文损坏）"))?;
        Ok(plain.to_vec())
    }

    /// aes-128-cbc-fixed：key = 16 位口令的 UTF-8 字节，IV 全 0，密文为纯 Base64。
    fn decrypt_fixed_iv(&self, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
        let key = self.passphrase.as_bytes();
        let iv = [0u8; 16];
        let mut buf = raw.to_vec();
        let plain = Aes128CbcDec::new(key.into(), &iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|_| anyhow::anyhow!("PKCS7 填充非法（口令错误或密文损坏）"))?;
        Ok(plain.to_vec())
    }

    /// 解密 Base64 密文 → JSON payload（按 crypto_type 选择算法）。
    fn decrypt(&self, encrypted_b64: &str, crypto_type: &str) -> anyhow::Result<DecryptedPayload> {
        let raw = base64::engine::general_purpose::STANDARD.decode(encrypted_b64)?;
        let plaintext = match crypto_type {
            "aes-128-cbc-fixed" => self.decrypt_fixed_iv(&raw)?,
            "legacy" => self.decrypt_legacy(&raw)?,
            // 服务端未标类型：按 `Salted__` 头判断（旧版服务端只产出 legacy）
            _ if raw.starts_with(b"Salted__") => self.decrypt_legacy(&raw)?,
            _ => self.decrypt_fixed_iv(&raw)?,
        };
        Ok(serde_json::from_slice(&plaintext)?)
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
            let decrypted = self.decrypt(&data.encrypted, &data.crypto_type)?;
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

    /// CryptoJS 口令：md5("{uuid}-{password}") 十六进制前 16 个字符。
    fn passphrase_of(uuid: &str, password: &str) -> String {
        let digest = md5::Md5::digest(format!("{uuid}-{password}").as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        hex[..16].to_string()
    }

    /// 构造 legacy 格式密文：CryptoJS 口令模式（Salted__ + EVP_BytesToKey(MD5) + AES-256-CBC）。
    fn encrypt_legacy_sample(passphrase: &str, plaintext: &str) -> String {
        use aes::cipher::BlockEncryptMut;
        type Enc = cbc::Encryptor<aes::Aes256>;
        let salt = [7u8; 8];
        let mut key_iv = Vec::new();
        let mut prev: Vec<u8> = Vec::new();
        while key_iv.len() < 48 {
            let mut input = prev.clone();
            input.extend_from_slice(passphrase.as_bytes());
            input.extend_from_slice(&salt);
            prev = md5::Md5::digest(&input).to_vec();
            key_iv.extend_from_slice(&prev);
        }
        let key: [u8; 32] = key_iv[..32].try_into().unwrap();
        let iv: [u8; 16] = key_iv[32..48].try_into().unwrap();
        let pt = plaintext.as_bytes();
        let mut buf = vec![0u8; pt.len() + 16];
        buf[..pt.len()].copy_from_slice(pt);
        let ct = Enc::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, pt.len())
            .unwrap()
            .to_vec();
        let mut raw = b"Salted__".to_vec();
        raw.extend_from_slice(&salt);
        raw.extend_from_slice(&ct);
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    /// 构造 aes-128-cbc-fixed 密文：key = 口令 16 字节，IV 全 0，纯 Base64。
    fn encrypt_fixed_iv_sample(passphrase: &str, plaintext: &str) -> String {
        use aes::cipher::BlockEncryptMut;
        type Enc = cbc::Encryptor<aes::Aes128>;
        let key = passphrase.as_bytes();
        let iv = [0u8; 16];
        let pt = plaintext.as_bytes();
        let mut buf = vec![0u8; pt.len() + 16];
        buf[..pt.len()].copy_from_slice(pt);
        let ct = Enc::new(key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, pt.len())
            .unwrap()
            .to_vec();
        base64::engine::general_purpose::STANDARD.encode(ct)
    }

    #[test]
    fn passphrase_matches_cookiecloud_spec() {
        // 官方算法：md5(uuid + '-' + password) 十六进制前 16 字符（ASCII，不是 MD5 原始字节）
        assert_eq!(
            CookieCloudClient::new("http://x", "test-uuid", "test-pass")
                .unwrap()
                .passphrase,
            "e0c4589a68c8b0ec"
        );
        // 回归夹具：独立算出的 md5(uuid-password) 前 16 位（与 CryptoJS 一致）
        assert_eq!(
            passphrase_of("fixture-uuid", "fixture-password"),
            "da25d18698f9b6fe"
        );
    }

    #[test]
    fn decrypt_legacy_roundtrip() {
        let uuid = "test-uuid";
        let password = "test-pass";
        let payload = r#"{"cookie_data": {"hdarea.club": [{"name": "uid", "value": "123"}, {"name": "pass", "value": "abc"}]}}"#;
        let encrypted = encrypt_legacy_sample(&passphrase_of(uuid, password), payload);

        let client = CookieCloudClient::new("http://x", uuid, password).unwrap();
        let decrypted = client.decrypt(&encrypted, "legacy").expect("解密失败");
        let cookies = &decrypted.cookie_data["hdarea.club"];
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "uid");
        assert_eq!(
            CookieCloudClient::cookies_to_string(cookies),
            "uid=123; pass=abc"
        );
        // 未标 crypto_type 时按 Salted__ 头自动识别
        assert!(client.decrypt(&encrypted, "").is_ok());
    }

    #[test]
    fn decrypt_fixed_iv_roundtrip() {
        let uuid = "test-uuid";
        let password = "test-pass";
        let payload = r#"{"cookie_data": {"pt.example.org": [{"name": "sid", "value": "s1"}]}}"#;
        let encrypted = encrypt_fixed_iv_sample(&passphrase_of(uuid, password), payload);
        let client = CookieCloudClient::new("http://x", uuid, password).unwrap();
        assert!(client.decrypt(&encrypted, "aes-128-cbc-fixed").is_ok());
        // 未标类型且无 Salted__ 头 → 按 fixed IV 处理
        assert!(client.decrypt(&encrypted, "").is_ok());
    }

    #[test]
    fn decrypt_rejects_wrong_passphrase() {
        let payload = r#"{"cookie_data": {}}"#;
        let encrypted = encrypt_legacy_sample(&passphrase_of("u", "right"), payload);
        let client = CookieCloudClient::new("http://x", "u", "wrong").unwrap();
        assert!(client.decrypt(&encrypted, "legacy").is_err());
        // 非密文数据同样拒绝
        let bad = base64::engine::general_purpose::STANDARD.encode(b"plaindata-not-encrypted");
        assert!(client.decrypt(&bad, "").is_err());
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
