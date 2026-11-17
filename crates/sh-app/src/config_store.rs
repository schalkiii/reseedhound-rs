//! 配置读写中心：GUI 侧编辑 config.yaml / sites.yaml 的唯一通道。
//!
//! 写回策略（对齐 savept.rs 的行级写回哲学）：
//! - **已存在的键**：就地改值，保留注释与顺序
//! - **缺失的键**：在所属块内补行（缩进从块内已有字段推断）
//! - **新站点**：追加到文件末尾（缩进从最后条目推断）
//! - 每次写回后用 `serde_yaml::from_str` 验证产物有效，无效则拒写并返回错误
//!
//! 与调度器的协作：调度器每轮 `run_cycle` 都从磁盘重建引擎（CookieCloud
//! 同步路径），因此 GUI 保存后**下一轮自动生效**，无需重启进程。

use std::path::{Path, PathBuf};

/// 就地更新或补建 `key: value` 行（顶层标量键）。
#[cfg(test)]
fn upsert_top_key(content: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    let mut found = false;
    for line in content.lines() {
        if !found {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix(&format!("{key}:")) {
                // 键行：冒号后可以是值或空（块级键则跳过）
                if rest.is_empty() || rest.starts_with(' ') {
                    out.push_str(&format!("{key}: {value}\n"));
                    found = true;
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if found {
        out
    } else {
        format!("{content}\n{key}: {value}\n")
    }
}

/// 顶层块（如 `downloader:`）内就地更新或补建字段行。
///
/// `field` 形如 `host` / `port`；`section` 形如 `downloader`。
/// 策略：定位到 `section:` 行后，在块内（下一个缩进更小或同级的顶层键前）
/// 找 field 行就地改；找不到则插到块内第一行前（保持块紧凑）。
fn upsert_section_field(content: &str, section: &str, field: &str, value: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let section_prefix = format!("{section}:");
    let field_prefix = format!("{field}:");

    // 1. 找 section 行
    let Some(sec_idx) = lines
        .iter()
        .position(|l| l.trim_start() == section_prefix || l.trim_start().starts_with(&format!("{section}: ")))
    else {
        // 无此 section：追加整块
        lines.push(String::new());
        lines.push(format!("{section}:"));
        lines.push(format!("  {field}: {value}"));
        return lines.join("\n") + "\n";
    };

    // 2. 在块内找 field 行（块内 = 缩进 > 0 且未遇到下一个顶层键）
    let mut insert_at = None;
    let mut field_idx = None;
    for (i, line) in lines.iter().enumerate().skip(sec_idx + 1) {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            // 到下一个顶层键了，块结束
            insert_at = Some(i);
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with(&field_prefix) {
            let rest = &trimmed[field_prefix.len()..];
            if rest.is_empty() || rest.starts_with(' ') {
                field_idx = Some(i);
                break;
            }
        }
    }
    if let Some(idx) = field_idx {
        let indent = "  ".to_string();
        lines[idx] = format!("{indent}{field}: {value}");
        return lines.join("\n") + "\n";
    }
    // 3. 补行：插到块内最后一条字段行之后（保持块紧凑）
    let insert_pos = insert_at.unwrap_or(lines.len());
    // 找块内最后一个非空非注释行
    let mut last_field_line = sec_idx;
    for (i, line) in lines.iter().enumerate().take(insert_pos).skip(sec_idx + 1) {
        if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            last_field_line = i;
        }
    }
    lines.insert(last_field_line + 1, format!("  {field}: {value}"));
    lines.join("\n") + "\n"
}

/// 站点条目字段就地更新：定位 `- name: <name>` 条目，在其字段区就地改值。
///
/// 若字段不存在则补行（缩进从该条目已有字段行推断）。
fn upsert_site_field(content: &str, name: &str, field: &str, value: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let field_prefix = format!("{field}:");

    let mut entry_start: Option<usize> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- name:") {
            let entry_name = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if entry_name == name {
                entry_start = Some(i);
                break;
            }
        }
        i += 1;
    }
    let Some(start) = entry_start else {
        return content.to_string(); // 未找到条目：不动（调用方保证先有条目）
    };

    // 条目边界：下一个 "- name:" 或文件尾
    let mut end = lines.len();
    for (j, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim_start().starts_with("- name:") {
            end = j;
            break;
        }
    }

    // 块内找 field 行
    let mut field_indent = 4usize;
    for (j, line) in lines.iter().enumerate().take(end).skip(start + 1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || line.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent > 2 {
            field_indent = indent;
        }
        if trimmed.starts_with(&field_prefix) {
            let rest = &trimmed[field_prefix.len()..];
            if rest.is_empty() || rest.starts_with(' ') {
                let indent_str = " ".repeat(indent);
                lines[j] = format!("{indent_str}{field}: {value}");
                return lines.join("\n") + "\n";
            }
        }
    }
    // 补行：插到条目最后一条字段行之后
    lines.insert(end, format!("{}{field}: {value}", " ".repeat(field_indent)));
    lines.join("\n") + "\n"
}

/// 追加新站点到文件末尾（或 sites: 块末尾）。
fn append_site(content: &str, name: &str, url: &str, passkey: &str, enabled: bool) -> String {
    let suffix = if enabled { "true" } else { "false" };
    let entry = format!(
        "  - name: {name}\n    url: {url}\n    passkey: {passkey}\n    enabled: {suffix}\n"
    );
    // 文件已有内容且以换行结尾 → 直接追加；否则补一个空行
    if content.is_empty() {
        format!("sites:\n{entry}")
    } else if content.ends_with('\n') {
        format!("{content}{entry}")
    } else {
        format!("{content}\n{entry}")
    }
}

/// YAML 产物有效性验证：解析失败则拒写。
fn ensure_valid_yaml(content: &str) -> anyhow::Result<()> {
    let value: serde_yaml::Value = serde_yaml::from_str(content)
        .map_err(|e| anyhow::anyhow!("写回的 YAML 无效: {e}"))?;
    if value.is_null() && !content.trim().is_empty() {
        anyhow::bail!("写回的 YAML 为空");
    }
    Ok(())
}

/// GUI 编辑会话：读取时解析为表单模型，保存时逐字段写回。
pub struct ConfigStore {
    config_path: PathBuf,
    sites_path: PathBuf,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DownloaderForm {
    pub dl_type: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub password: String,
    pub torrent_dir: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SiteForm {
    pub name: String,
    pub url: String,
    pub passkey: String,
    pub api_url: String,
    pub enabled: bool,
}

impl ConfigStore {
    pub fn new(config_path: &Path, sites_path: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            sites_path: sites_path.to_path_buf(),
        }
    }

    #[allow(dead_code)]
    pub fn config_exists(&self) -> bool {
        self.config_path.exists() && self.sites_path.exists()
    }

    /// 读取当前配置文件原文（YAML 文本）。
    pub fn read_config_text(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(&self.config_path)?)
    }

    pub fn read_sites_text(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(&self.sites_path)?)
    }

    /// 从模板初始化配置（首启引导用）。
    pub fn init_from_example(&self) -> anyhow::Result<()> {
        let cfg_template = std::include_str!("../assets/config.example.yaml");
        let sites_template = std::include_str!("../assets/sites.example.yaml");
        if !self.config_path.exists() {
            std::fs::write(&self.config_path, cfg_template)?;
        }
        if !self.sites_path.exists() {
            std::fs::write(&self.sites_path, sites_template)?;
        }
        Ok(())
    }

    /// 保存下载器表单 → config.yaml（行级写回，保留注释）。
    pub fn save_downloader(&self, form: &DownloaderForm) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = content.clone();
        next = upsert_section_field(&next, "downloader", "type", &form.dl_type);
        next = upsert_section_field(&next, "downloader", "host", &form.host);
        next = upsert_section_field(&next, "downloader", "port", &form.port);
        next = upsert_section_field(&next, "downloader", "username", &form.username);
        next = upsert_section_field(&next, "downloader", "port", &form.port);
        next = upsert_section_field(&next, "downloader", "password", &form.password);
        next = upsert_section_field(&next, "downloader", "torrent_dir", &form.torrent_dir);
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存站点表单集合 → sites.yaml（逐字段行级写回）。
    pub fn save_sites(&self, sites: &[SiteForm]) -> anyhow::Result<()> {
        let content = self.read_sites_text()?;
        let mut next = content.clone();
        for site in sites {
            if site.name.is_empty() {
                continue;
            }
            // 检查是否已有此条目（按 name 定位）
            let exists = next
                .lines()
                .any(|l| l.trim_start().strip_prefix("- name:")
                    .map(|r| r.trim().trim_matches('"').trim_matches('\'').trim() == site.name)
                    .unwrap_or(false));
            if !exists {
                next = append_site(&next, &site.name, &site.url, &site.passkey, site.enabled);
            }
            next = upsert_site_field(&next, &site.name, "url", &site.url);
            next = upsert_site_field(&next, &site.name, "passkey", &site.passkey);
            let flag = if site.enabled { "true" } else { "false" };
            next = upsert_site_field(&next, &site.name, "enabled", flag);
            if !site.api_url.is_empty() {
                next = upsert_site_field(&next, &site.name, "api_url", &site.api_url);
            }
        }
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.sites_path, next)?;
        Ok(())
    }

    /// 保存调度设置 → config.yaml。
    pub fn save_scheduler(&self, enabled: bool, interval_minutes: f64, run_on_start: bool) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = upsert_section_field(&content, "scheduler", "enabled", if enabled { "true" } else { "false" });
        next = upsert_section_field(&next, "scheduler", "interval_minutes", &format!("{interval_minutes}"));
        next = upsert_section_field(&next, "scheduler", "run_on_start", if run_on_start { "true" } else { "false" });
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存全局参数 → config.yaml。
    pub fn save_global(&self, concurrency: usize, batch_size: usize, mode: &str) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = upsert_section_field(&content, "global", "concurrency", &concurrency.to_string());
        next = upsert_section_field(&next, "global", "batch_size", &batch_size.to_string());
        next = upsert_section_field(&next, "global", "mode", mode);
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 解析 sites.yaml 为表单模型（列表页展示用）。
    pub fn load_sites_form(&self) -> anyhow::Result<Vec<SiteForm>> {
        let text = self.read_sites_text()?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text)?;
        let Some(list) = parsed.get("sites").and_then(|v| v.as_sequence()) else {
            return Ok(Vec::new());
        };
        Ok(list
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_string();
                let url = entry.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let passkey = entry.get("passkey").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let api_url = entry.get("api_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let enabled = entry.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                Some(SiteForm { name, url, passkey, api_url, enabled })
            })
            .collect())
    }

    /// 解析 config.yaml 的 downloader 块为表单模型。
    pub fn load_downloader_form(&self) -> anyhow::Result<DownloaderForm> {
        let text = self.read_config_text()?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text)?;
        let get = |k: &str| -> String {
            parsed
                .get("downloader")
                .and_then(|d| d.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(DownloaderForm {
            dl_type: get("type"),
            host: get("host"),
            port: get("port"),
            username: get("username"),
            password: get("password"),
            torrent_dir: get("torrent_dir"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = r#"# 主配置
global:
  concurrency: 20   # 并发
  mode: pieces_hash

downloader:
  type: qbittorrent
  host: 127.0.0.1
  port: 8080
  username: admin
  password: old
  torrent_dir: D:/dl
"#;

    const SAMPLE_SITES: &str = r#"# 站点
sites:
  - name: siteA
    url: https://a.com
    passkey: pk1
    enabled: true

  - name: siteB
    url: https://b.com
    passkey: pk2
    # enabled 缺省
"#;

    #[test]
    fn upsert_section_updates_in_place() {
        let next = upsert_section_field(SAMPLE_CONFIG, "downloader", "password", "new");
        assert!(next.contains("password: new"));
        assert!(!next.contains("password: old"));
        // 注释保留
        assert!(next.contains("# 并发"));
        assert!(next.contains("# 主配置"));
    }

    #[test]
    fn upsert_section_appends_missing_section() {
        let next = upsert_section_field("global:\n  mode: pieces_hash\n", "jackett", "enabled", "false");
        assert!(next.contains("jackett:"));
        assert!(next.contains("enabled: false"));
        let v: serde_yaml::Value = serde_yaml::from_str(&next).unwrap();
        assert_eq!(v.get("jackett").unwrap().get("enabled").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn upsert_site_field_updates_in_place() {
        let next = upsert_site_field(SAMPLE_SITES, "siteB", "enabled", "false");
        assert!(next.contains("enabled: false"));
        assert!(next.contains("# enabled 缺省"));
        let v: serde_yaml::Value = serde_yaml::from_str(&next).unwrap();
        let sites = v.get("sites").unwrap().as_sequence().unwrap();
        assert_eq!(sites.len(), 2);
    }

    #[test]
    fn append_site_keeps_valid() {
        let next = append_site(SAMPLE_SITES, "siteC", "https://c.com", "pk3", true);
        let v: serde_yaml::Value = serde_yaml::from_str(&next).unwrap();
        let sites = v.get("sites").unwrap().as_sequence().unwrap();
        assert_eq!(sites.len(), 3);
        assert_eq!(sites[2].get("name").unwrap().as_str(), Some("siteC"));
    }

    #[test]
    fn upsert_top_key_appends_when_missing() {
        let next = upsert_top_key("global:\n  mode: pieces_hash\n", "db", "path: data/x.db");
        assert!(next.contains("db: path: data/x.db"));
    }
}
