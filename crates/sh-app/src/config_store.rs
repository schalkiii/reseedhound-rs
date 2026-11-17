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
fn append_site(content: &str, name: &str, url: &str, passkey: &str, enabled: bool) -> String {    let suffix = if enabled { "true" } else { "false" };
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

/// 从 sites.yaml 移除未保留的站点条目（整条 `- name: …` 块，含注释行随块删除）。
/// `kept` 是要保留的站点名集合。
fn remove_site_entries(content: &str, kept: &std::collections::HashSet<&str>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("- name:") {
            let name = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !kept.contains(name) {
                skipping = true;
                continue;
            }
            skipping = false;
            out.push(line.to_string());
            continue;
        }
        if skipping {
            // 条目内：缩进 > 0 的行、紧邻的空行都跳过；回到顶层（缩进 0）即停
            if line.trim().is_empty() || line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }
            skipping = false;
        }
        out.push(line.to_string());
    }
    // 去掉尾部多余空行
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n") + "\n"
}

/// 确保两级嵌套块 `parent:` → `  child:` 存在（存在则原样返回）。
fn ensure_nested_section(content: &str, parent: &str, child: &str) -> String {
    // 已有 parent 块？
    let has_parent = content.lines().any(|l| {
        let t = l.trim_start();
        t == format!("{parent}:") || t.starts_with(&format!("{parent}: "))
    });
    if !has_parent {
        let mut out = content.trim_end().to_string();
        out.push_str(&format!("\n\n{parent}:\n  {child}:\n"));
        return out;
    }
    // parent 块内已有 child？
    let parent_prefix = format!("{parent}:");
    let mut in_parent = false;
    for line in content.lines() {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 0 {
            in_parent = t == parent_prefix || t.starts_with(&format!("{parent}: "));
            continue;
        }
        if in_parent && (t == format!("{child}:") || t.starts_with(&format!("{child}: "))) {
            return content.to_string();
        }
    }
    // 有 parent 无 child：在 parent 行后插入 child 行
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if let Some(idx) = lines
        .iter()
        .position(|l| l.trim_start() == parent_prefix || l.trim_start().starts_with(&format!("{parent}: ")))
    {
        lines.insert(idx + 1, format!("  {child}:"));
    }
    lines.join("\n") + "\n"
}

/// 嵌套块内字段就地更新（`parent:` → `  child:` → `    field: value`）。
fn upsert_nested_field(content: &str, parent: &str, child: &str, field: &str, value: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let parent_prefix = format!("{parent}:");
    let child_prefix = format!("{child}:");
    let field_prefix = format!("{field}:");

    // 1. 定位 parent（缩进 0）
    let Some(p_idx) = lines.iter().position(|l| {
        let t = l.trim_start();
        t == parent_prefix || t.starts_with(&format!("{parent}: "))
    }) else {
        return content.to_string();
    };
    // 2. 定位 child（parent 块内，缩进 > 0 且 < 下一顶层键）
    let mut c_idx = None;
    let mut insert_after = lines.len();
    for (i, line) in lines.iter().enumerate().skip(p_idx + 1) {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 0 && !t.is_empty() && !t.starts_with('#') {
            insert_after = i;
            break;
        }
        if indent > 0
            && (t == child_prefix || t.starts_with(&format!("{child}: ")))
        {
            c_idx = Some(i);
            break;
        }
    }
    let Some(c_start) = c_idx else {
        return content.to_string(); // ensure_nested_section 应已建好
    };
    // 3. child 块内找 field（缩进 > child 的缩进）
    let child_indent = lines[c_start].len() - lines[c_start].trim_start().len();
    let mut end = insert_after;
    for (i, line) in lines.iter().enumerate().skip(c_start + 1) {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if !t.is_empty() && !t.starts_with('#') && indent <= child_indent {
            end = i;
            break;
        }
    }
    for (_i, line) in lines.iter_mut().enumerate().take(end).skip(c_start + 1) {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent > child_indent && t.starts_with(&field_prefix) {
            let rest = &t[field_prefix.len()..];
            if rest.is_empty() || rest.starts_with(' ') {
                *line = format!("{}{field}: {value}", " ".repeat(child_indent + 2));
                return lines.join("\n") + "\n";
            }
        }
    }
    // 4. 补行到 child 块内最后
    let mut last = c_start;
    for (i, line) in lines.iter().enumerate().take(end).skip(c_start + 1) {
        if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            last = i;
        }
    }
    lines.insert(last + 1, format!("{}{field}: {value}", " ".repeat(child_indent + 2)));
    lines.join("\n") + "\n"
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
    /// 行为开关（缺省 true / true / SeedHound，空串表示不改）
    pub tag: String,
    pub skip_hash_check: Option<bool>,
    pub auto_start: Option<bool>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FeishuForm {
    pub webhook_url: String,
    pub secret: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CookieCloudForm {
    pub enabled: bool,
    pub url: String,
    pub uuid: String,
    pub password: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct JackettForm {
    pub enabled: bool,
    pub url: String,
    pub api_key: String,
    pub concurrency: String,
    pub timeout: String,
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
        next = upsert_section_field(&next, "downloader", "password", &form.password);
        next = upsert_section_field(&next, "downloader", "torrent_dir", &form.torrent_dir);
        if !form.tag.trim().is_empty() {
            next = upsert_section_field(&next, "downloader", "tag", form.tag.trim());
        }
        if let Some(skip) = form.skip_hash_check {
            let v = if skip { "true" } else { "false" };
            next = upsert_section_field(&next, "downloader", "skip_hash_check", v);
        }
        if let Some(auto) = form.auto_start {
            let v = if auto { "true" } else { "false" };
            next = upsert_section_field(&next, "downloader", "auto_start", v);
        }
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存站点表单集合 → sites.yaml（逐字段行级写回）。
    /// GUI 列表中已被删除的站点（yaml 有而表单无）一并从 yaml 移除。
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
        // 移除 GUI 列表中已删除的条目
        let kept: std::collections::HashSet<&str> =
            sites.iter().filter(|s| !s.name.is_empty()).map(|s| s.name.as_str()).collect();
        next = remove_site_entries(&next, &kept);
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

    /// 保存飞书通知 → config.yaml（notify.feishu 块）。
    pub fn save_feishu(&self, form: &FeishuForm) -> anyhow::Result<()> {
        // notify.feishu 是嵌套块：先确保 notify: 与 feishu: 存在，再写子字段。
        // 简化路径：用 upsert_section_field 处理不了两级嵌套，这里做定向嵌套写。
        let content = self.read_config_text()?;
        let mut next = ensure_nested_section(&content, "notify", "feishu");
        let field = |next: String, k: &str, v: &str| -> String {
            upsert_nested_field(&next, "notify", "feishu", k, v)
        };
        next = field(next, "webhook_url", &form.webhook_url);
        next = field(next, "secret", &form.secret);
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存 CookieCloud → config.yaml。
    pub fn save_cookiecloud(&self, form: &CookieCloudForm) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = content.clone();
        let flag = if form.enabled { "true" } else { "false" };
        next = upsert_section_field(&next, "cookiecloud", "enabled", flag);
        next = upsert_section_field(&next, "cookiecloud", "url", &form.url);
        next = upsert_section_field(&next, "cookiecloud", "uuid", &form.uuid);
        next = upsert_section_field(&next, "cookiecloud", "password", &form.password);
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存 Jackett → config.yaml。
    pub fn save_jackett(&self, form: &JackettForm) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = content.clone();
        let flag = if form.enabled { "true" } else { "false" };
        next = upsert_section_field(&next, "jackett", "enabled", flag);
        next = upsert_section_field(&next, "jackett", "url", &form.url);
        next = upsert_section_field(&next, "jackett", "api_key", &form.api_key);
        if !form.concurrency.trim().is_empty() {
            next = upsert_section_field(&next, "jackett", "concurrency", form.concurrency.trim());
        }
        if !form.timeout.trim().is_empty() {
            next = upsert_section_field(&next, "jackett", "timeout", form.timeout.trim());
        }
        ensure_valid_yaml(&next)?;
        std::fs::write(&self.config_path, next)?;
        Ok(())
    }

    /// 保存日志设置 → config.yaml（log.level / log.file）。
    pub fn save_log(&self, level: &str, file: &str) -> anyhow::Result<()> {
        let content = self.read_config_text()?;
        let mut next = upsert_section_field(&content, "log", "level", level);
        next = upsert_section_field(&next, "log", "file", file);
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
        let flag = |k: &str| -> Option<bool> {
            parsed
                .get("downloader")
                .and_then(|d| d.get(k))
                .and_then(|v| v.as_bool())
        };
        Ok(DownloaderForm {
            dl_type: get("type"),
            host: get("host"),
            port: get("port"),
            username: get("username"),
            password: get("password"),
            torrent_dir: get("torrent_dir"),
            tag: get("tag"),
            skip_hash_check: flag("skip_hash_check"),
            auto_start: flag("auto_start"),
        })
    }

    /// 解析 notify.feishu 块。
    pub fn load_feishu_form(&self) -> anyhow::Result<FeishuForm> {
        let parsed: serde_yaml::Value = serde_yaml::from_str(&self.read_config_text()?)?;
        let block = parsed.get("notify").and_then(|n| n.get("feishu"));
        let get = |k: &str| -> String {
            block
                .and_then(|b| b.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(FeishuForm {
            webhook_url: get("webhook_url"),
            secret: get("secret"),
        })
    }

    /// 解析 cookiecloud 块。
    pub fn load_cookiecloud_form(&self) -> anyhow::Result<CookieCloudForm> {
        let parsed: serde_yaml::Value = serde_yaml::from_str(&self.read_config_text()?)?;
        let block = parsed.get("cookiecloud");
        let enabled = block
            .and_then(|b| b.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let get = |k: &str| -> String {
            block
                .and_then(|b| b.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(CookieCloudForm {
            enabled,
            url: get("url"),
            uuid: get("uuid"),
            password: get("password"),
        })
    }

    /// 解析 jackett 块。
    pub fn load_jackett_form(&self) -> anyhow::Result<JackettForm> {
        let parsed: serde_yaml::Value = serde_yaml::from_str(&self.read_config_text()?)?;
        let block = parsed.get("jackett");
        let enabled = block
            .and_then(|b| b.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let get_str = |k: &str| -> String {
            block
                .and_then(|b| b.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let get_num = |k: &str, d: &str| -> String {
            block
                .and_then(|b| b.get(k))
                .and_then(|v| {
                    v.as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_f64().map(|f| f.to_string()))
                })
                .unwrap_or_else(|| d.to_string())
        };
        Ok(JackettForm {
            enabled,
            url: get_str("url"),
            api_key: get_str("api_key"),
            concurrency: get_num("concurrency", "5"),
            timeout: get_num("timeout", "30"),
        })
    }

    /// 解析 log 块。
    pub fn load_log_form(&self) -> anyhow::Result<(String, String)> {
        let parsed: serde_yaml::Value = serde_yaml::from_str(&self.read_config_text()?)?;
        let block = parsed.get("log");
        let get = |k: &str, d: &str| -> String {
            block
                .and_then(|b| b.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or(d)
                .to_string()
        };
        Ok((get("level", "INFO"), get("file", "logs/reseed.log")))
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

    #[test]
    fn nested_field_creates_and_updates() {
        // 无 notify 块：创建
        let created = ensure_nested_section("global:\n  mode: pieces_hash\n", "notify", "feishu");
        assert!(created.contains("notify:"));
        assert!(created.contains("feishu:"));
        // 写字段
        let with_field = upsert_nested_field(&created, "notify", "feishu", "webhook_url", "https://open.feishu.cn/hook/x");
        let v: serde_yaml::Value = serde_yaml::from_str(&with_field).unwrap();
        assert_eq!(
            v.get("notify").unwrap().get("feishu").unwrap().get("webhook_url").unwrap().as_str(),
            Some("https://open.feishu.cn/hook/x")
        );
        // 就地更新
        let updated = upsert_nested_field(&with_field, "notify", "feishu", "webhook_url", "https://other");
        assert!(updated.contains("https://other"));
        assert!(!updated.contains("hook/x"));
    }

    #[test]
    fn nested_field_preserves_sibling_sections() {
        let cfg = "global:\n  mode: pieces_hash\n\nnotify:\n  feishu:\n    webhook_url: old\n\njackett:\n  enabled: false\n";
        let next = upsert_nested_field(cfg, "notify", "feishu", "webhook_url", "new");
        let v: serde_yaml::Value = serde_yaml::from_str(&next).unwrap();
        assert_eq!(v.get("jackett").unwrap().get("enabled").unwrap().as_bool(), Some(false));
        assert_eq!(
            v.get("notify").unwrap().get("feishu").unwrap().get("webhook_url").unwrap().as_str(),
            Some("new")
        );
    }

    #[test]
    fn remove_site_entries_drops_unkept() {
        let mut kept = std::collections::HashSet::new();
        kept.insert("siteA");
        let next = remove_site_entries(SAMPLE_SITES, &kept);
        assert!(next.contains("siteA"));
        assert!(!next.contains("siteB"));
        assert!(next.contains("# 站点"));
        let v: serde_yaml::Value = serde_yaml::from_str(&next).unwrap();
        let sites = v.get("sites").unwrap().as_sequence().unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].get("name").unwrap().as_str(), Some("siteA"));
    }

    #[test]
    fn remove_site_entries_all_kept_noop() {
        let mut kept = std::collections::HashSet::new();
        kept.insert("siteA");
        kept.insert("siteB");
        let next = remove_site_entries(SAMPLE_SITES, &kept);
        assert!(next.contains("siteA"));
        assert!(next.contains("siteB"));
    }
}
