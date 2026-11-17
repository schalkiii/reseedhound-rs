//! savept.icu（PT监护室）死亡站点数据源与本地站点对照。
//!
//! 对齐 Python src/network/savept.py：
//! - GET {base}/api/history/dead?page={n} 分页拉取（has_more 控制，≤50 页）
//! - 匹配优先级：域名归一化一致 → 注册域一致（high），站点名/别名归一化一致（medium）
//! - 只出建议不改配置；写入需 CLI `check-dead --apply` 显式确认
//! - `disable_sites_in_yaml` 用文本行处理保留注释与顺序，杜绝 YAML 重复键

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use sh_core::config::SiteEntry;

pub const DEFAULT_SAVEPT_URL: &str = "https://savept.icu";
pub const DEAD_API_PATH: &str = "/api/history/dead";

/// savept 记录的一条死亡事件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeadSite {
    pub name: String,
    pub url: String,
    pub dead_date: String,
    pub event_label: String,
    pub alias: String,
}

/// 本地站点与死亡名单的一条匹配结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DeadMatch {
    pub local_name: String,
    pub local_url: String,
    pub dead_name: String,
    pub dead_url: String,
    pub dead_date: String,
    pub event_label: String,
    pub matched_by: &'static str,
    pub confidence: &'static str, // high / medium
}

/// 把 URL 归一化成裸域名（去协议/www./端口/路径，小写）。
pub fn norm_domain(url: &str) -> String {
    let d = url.trim();
    // 去协议
    let d = match d.find("://") {
        Some(i) => &d[i + 3..],
        None => d,
    };
    // 去路径/查询/凭据/端口
    let d = d.split('/').next().unwrap_or("");
    let d = d.split('?').next().unwrap_or("");
    let d = d.rsplit('@').next().unwrap_or("");
    let d = d.split(':').next().unwrap_or("");
    let d = d.to_lowercase();
    d.strip_prefix("www.").unwrap_or(&d).to_string()
}

/// 粗略取注册域（末两段），不引入 publicsuffix 依赖。
pub fn registrable_domain(domain: &str) -> String {
    let parts: Vec<&str> = domain.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        domain.to_string()
    }
}

/// 站点名归一化：去空白、小写、去尾部 pt/tracker（MomentPT ↔ Moment）。
pub fn norm_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    s.strip_suffix("pt")
        .or_else(|| s.strip_suffix("tracker"))
        .unwrap_or(&s)
        .to_string()
}

#[derive(Debug, Deserialize)]
struct DeadPage {
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    items: Vec<DeadItem>,
}

#[derive(Debug, Deserialize)]
struct DeadItem {
    #[serde(default)]
    url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    event_label: String,
    #[serde(default)]
    description: String,
}

impl DeadItem {
    fn into_dead_site(self) -> DeadSite {
        DeadSite {
            name: self.name.trim().to_string(),
            url: self.url.trim().to_string(),
            dead_date: self.date.trim().to_string(),
            event_label: self.event_label.trim().to_string(),
            alias: self.description.trim().to_string(),
        }
    }
}

/// 分页拉取 savept 的死亡事件列表（解析失败/非 200/超时即停止并返回已收集部分）。
pub async fn fetch_dead_sites(
    base_url: &str,
    timeout_secs: f64,
    max_pages: u32,
) -> anyhow::Result<Vec<DeadSite>> {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        DEFAULT_SAVEPT_URL
    } else {
        base
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_secs.max(1.0)))
        .user_agent("ReseedHound-RS/0.1")
        .build()?;

    let mut collected = Vec::new();
    for page in 1..=max_pages {
        let url = format!("{base}{DEAD_API_PATH}?page={page}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("savept 死亡名单拉取错误 ({url}): {e}");
                break;
            }
        };
        if !resp.status().is_success() {
            tracing::warn!("savept 死亡名单拉取失败: HTTP {} ({url})", resp.status());
            break;
        }
        let payload: DeadPage = match resp.json().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("savept 返回非 JSON，停止拉取 ({url}): {e}");
                break;
            }
        };
        let has_more = payload.has_more;
        let batch_len = payload.items.len();
        collected.extend(payload.items.into_iter().map(DeadItem::into_dead_site));
        if !has_more || batch_len == 0 {
            break;
        }
    }
    Ok(collected)
}

/// 把死亡名单与本地站点对照，返回匹配结果（含置信度）。
pub fn match_local(dead_sites: &[DeadSite], local_sites: &[SiteEntry]) -> Vec<DeadMatch> {
    let mut by_domain: HashMap<String, &DeadSite> = HashMap::new();
    let mut by_reg: HashMap<String, &DeadSite> = HashMap::new();
    let mut by_name: HashMap<String, &DeadSite> = HashMap::new();

    for d in dead_sites {
        let dom = norm_domain(&d.url);
        if !dom.is_empty() {
            by_domain.entry(dom.clone()).or_insert(d);
            by_reg.entry(registrable_domain(&dom)).or_insert(d);
        }
        for raw in [&d.name, &d.alias] {
            let key = norm_name(raw);
            if !key.is_empty() {
                by_name.entry(key).or_insert(d);
            }
        }
    }

    let mut matches = Vec::new();
    for s in local_sites {
        let local_name = s.name.trim().to_string();
        let local_url = s.url.trim().to_string();
        let dom = norm_domain(&local_url);

        let mut matched: Option<(&DeadSite, &str)> = None;
        if !dom.is_empty() {
            if let Some(d) = by_domain.get(&dom) {
                matched = Some((d, "domain"));
            } else if let Some(d) = by_reg.get(&registrable_domain(&dom)) {
                matched = Some((d, "registrable"));
            }
        }
        if matched.is_none() {
            let key = norm_name(&local_name);
            if !key.is_empty() {
                if let Some(d) = by_name.get(&key) {
                    matched = Some((d, "name"));
                }
            }
        }
        let Some((dead, matched_by)) = matched else {
            continue;
        };

        let confidence = if matches!(matched_by, "domain" | "registrable") {
            "high"
        } else {
            "medium"
        };
        matches.push(DeadMatch {
            local_name,
            local_url,
            dead_name: dead.name.clone(),
            dead_url: dead.url.clone(),
            dead_date: dead.dead_date.clone(),
            event_label: dead.event_label.clone(),
            matched_by,
            confidence,
        });
    }
    matches
}

/// 把匹配结果排版成可直接看的建议清单。
pub fn format_report(matches: &[DeadMatch], total_local: usize) -> String {
    if matches.is_empty() {
        return format!("未发现本地 {total_local} 个站点出现在 savept.icu 死亡名单中，无需禁用。");
    }
    let mut lines = vec![
        format!(
            "对照 savept.icu 死亡名单，本地 {total_local} 个站点中有 {} 个命中：",
            matches.len()
        ),
        String::new(),
        format!(
            "{:<12}{:<7}{:<12}{:<10}{:<14}{}",
            "本地站点", "置信", "死亡日期", "死因", "savept 名称", "匹配"
        ),
        "-".repeat(78),
    ];
    let mut sorted = matches.to_vec();
    sorted.sort_by(|a, b| {
        // high 优先，同类按本地站点名
        let ca = if a.confidence == "high" { 0 } else { 1 };
        let cb = if b.confidence == "high" { 0 } else { 1 };
        ca.cmp(&cb).then_with(|| a.local_name.cmp(&b.local_name))
    });
    for m in &sorted {
        lines.push(format!(
            "{:<12}{:<7}{:<12}{:<10}{:<14}{}",
            m.local_name, m.confidence, m.dead_date, m.event_label, m.dead_name, m.matched_by
        ));
    }
    lines.push(String::new());
    lines.push("建议动作：".to_string());
    lines.push("  - confidence=high   → 可直接禁用（站点确已死亡）".to_string());
    lines.push("  - confidence=medium → 域名不同，先人工核对是否站点换域名再决定".to_string());
    lines.push("  - 写入配置请加 --apply（默认只打印建议，不改 sites.yaml）".to_string());
    lines.join("\n")
}

/// 把指定站点写成 enabled: false（保留条目便于恢复）。返回实际改动的站点名。
///
/// 文本行处理而非 yaml 转储，保留注释与顺序；条目已有 enabled 键则就地改值，
/// 杜绝重复键（YAML 重复键取后者会导致禁用失效）。
pub fn disable_sites_in_yaml(sites_path: &str, names: &[String]) -> anyhow::Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let path = Path::new(sites_path);
    if !path.exists() {
        anyhow::bail!("站点配置不存在: {sites_path}");
    }
    let targets: std::collections::HashSet<&str> = names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .collect();

    let content = std::fs::read_to_string(path)?;
    let mut out = String::with_capacity(content.len() + 64);
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut current: Option<String> = None;
    let mut current_has_enabled = false;
    // 当前条目字段缩进（从已有字段行推断；无字段时默认 4 空格 = "- name:" 缩进 + 2）
    let mut current_field_indent: usize = 4;

    let flush = |out: &mut String,
                 current: &Option<String>,
                 current_has_enabled: bool,
                 field_indent: usize,
                 done: &mut std::collections::HashSet<String>| {
        // 条目结束：目标站点若还没有 enabled 行，补一条（缩进与字段对齐，保证 YAML 有效）
        if let Some(cur) = current {
            if targets.contains(cur.as_str()) && !done.contains(cur) && !current_has_enabled {
                out.push_str(&" ".repeat(field_indent));
                out.push_str("enabled: false\n");
                done.insert(cur.clone());
            }
        }
    };

    for line in content.lines() {
        let line_with_nl = format!("{line}\n");
        // 顶级列表条目："- name: xxx"
        if let Some(rest) = line.trim_start().strip_prefix("- name:") {
            flush(
                &mut out,
                &current,
                current_has_enabled,
                current_field_indent,
                &mut done,
            );
            let name = rest.trim().trim_matches('"').trim_matches('\'').trim();
            current = Some(name.to_string());
            current_has_enabled = false;
            current_field_indent = 4;
            out.push_str(&line_with_nl);
            continue;
        }
        // 条目字段行 → 推断缩进
        let trimmed = line.trim_start();
        if !trimmed.is_empty() && !line.starts_with('#') {
            let indent_len = line.len() - trimmed.len();
            if indent_len > 2 {
                current_field_indent = indent_len;
            }
        }
        // enabled 键行
        if let Some((indent, _value)) = parse_enabled_line(line) {
            if let Some(cur) = &current {
                if targets.contains(cur.as_str()) {
                    // 就地改值，不新增行
                    out.push_str(&format!("{indent}enabled: false\n"));
                    current_has_enabled = true;
                    done.insert(cur.clone());
                    continue;
                }
            }
            current_has_enabled = true;
        }
        out.push_str(&line_with_nl);
    }
    flush(
        &mut out,
        &current,
        current_has_enabled,
        current_field_indent,
        &mut done,
    );

    std::fs::write(path, out)?;
    let mut result: Vec<String> = done.into_iter().collect();
    result.sort();
    Ok(result)
}

/// 解析 `enabled: xxx` 行 → (缩进, 原值)。
fn parse_enabled_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let after_key = trimmed.strip_prefix("enabled")?;
    // 键后必须是冒号 + 空白
    let after_colon = after_key.strip_prefix(':')?;
    if !after_colon.starts_with(char::is_whitespace) && !after_colon.is_empty() {
        return None;
    }
    let indent_len = line.len() - trimmed.len();
    Some((" ".repeat(indent_len), after_colon.trim().to_string()))
}

/// 读取 sites.yaml 的站点清单（only_enabled=true 跳过已禁用站点）。
pub fn load_local_sites(sites_path: &str, only_enabled: bool) -> anyhow::Result<Vec<SiteEntry>> {
    let path = Path::new(sites_path);
    if !path.exists() {
        anyhow::bail!("站点配置不存在: {sites_path}");
    }
    let raw: sh_core::config::SitesFile = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(raw
        .sites
        .into_iter()
        .filter(|s| !s.name.trim().is_empty() && !s.url.trim().is_empty())
        .filter(|s| {
            // enable 为历史拼写，优先级高于 enabled
            let enabled = s.enable.or(s.enabled).unwrap_or(true);
            !only_enabled || enabled
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_domain_strips_everything() {
        assert_eq!(
            norm_domain("https://www.Example.com:8080/path?q=1"),
            "example.com"
        );
        assert_eq!(norm_domain("https://pt.cdfile.org/"), "pt.cdfile.org");
        assert_eq!(norm_domain("https://user@host.io/x"), "host.io");
        assert_eq!(norm_domain(""), "");
    }

    #[test]
    fn registrable_domain_takes_last_two() {
        assert_eq!(registrable_domain("pt.cdfile.org"), "cdfile.org");
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("single"), "single");
    }

    #[test]
    fn norm_name_strips_pt_suffix() {
        assert_eq!(norm_name("MomentPT"), "moment");
        assert_eq!(norm_name("Some Tracker"), "some");
        assert_eq!(norm_name("keep"), "keep");
    }

    #[test]
    fn match_local_domain_and_name() {
        let dead = vec![
            DeadSite {
                name: "MomentPT".into(),
                url: "https://www.momentpt.top/".into(),
                dead_date: "2026-10-29".into(),
                event_label: "意外死亡".into(),
                alias: "瞬间".into(),
            },
            DeadSite {
                name: "Foo".into(),
                url: "https://foo.org".into(),
                dead_date: "2026-09-01".into(),
                event_label: "关站".into(),
                alias: String::new(),
            },
        ];
        let local = vec![
            SiteEntry {
                name: "Moment".into(),
                url: "https://momentpt.top/".into(),
                ..Default::default()
            },
            SiteEntry {
                name: "FooBar".into(),
                url: "https://sub.foo.org/".into(),
                ..Default::default()
            },
            SiteEntry {
                name: "Alive".into(),
                url: "https://alive.net".into(),
                ..Default::default()
            },
        ];
        let matches = match_local(&dead, &local);
        assert_eq!(matches.len(), 2);
        // momentpt.top 归一化后与 dead url 域名一致 → domain 优先
        assert_eq!(matches[0].matched_by, "domain");
        assert_eq!(matches[0].confidence, "high");
        // sub.foo.org 注册域 foo.org 与 dead 一致 → registrable
        assert_eq!(matches[1].matched_by, "registrable");
        assert_eq!(matches[1].confidence, "high");
    }

    #[test]
    fn report_empty_and_hit() {
        let r = format_report(&[], 10);
        assert!(r.contains("无需禁用"));
        let m = DeadMatch {
            local_name: "Foo".into(),
            local_url: "https://foo.org".into(),
            dead_name: "Foo".into(),
            dead_url: "https://foo.org".into(),
            dead_date: "2026-09-01".into(),
            event_label: "关站".into(),
            matched_by: "domain",
            confidence: "high",
        };
        let r = format_report(&[m], 10);
        assert!(r.contains("1 个命中"));
        assert!(r.contains("--apply"));
    }

    #[test]
    fn disable_preserves_comments_and_inplace() {
        let dir = std::env::temp_dir().join("sh_savept_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sites.yaml");
        std::fs::write(
            &p,
            "# 顶部注释\nsites:\n  - name: Foo\n    url: https://foo.org\n    passkey: \"k\"\n  - name: Bar\n    url: https://bar.net\n    enabled: true\n",
        )
        .unwrap();
        let changed =
            disable_sites_in_yaml(p.to_str().unwrap(), &["Foo".into(), "Bar".into()]).unwrap();
        assert_eq!(changed, vec!["Bar".to_string(), "Foo".to_string()]);
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# 顶部注释"), "注释保留");
        assert!(out.contains("passkey: \"k\""), "字段保留");
        // Foo：无 enabled 行 → 条目末尾按字段缩进（4 空格）补一条；Bar：已有 enabled → 就地改
        assert!(out.contains("    passkey: \"k\"\n    enabled: false\n"));
        assert!(out.contains("    url: https://bar.net\n    enabled: false\n"));
        // 不重复键
        assert_eq!(out.matches("enabled:").count(), 2);
        // 产物必须是有效 YAML 且禁用生效
        let parsed: sh_core::config::SitesFile = serde_yaml::from_str(&out).unwrap();
        assert_eq!(parsed.sites[0].enabled, Some(false));
        assert_eq!(parsed.sites[1].enabled, Some(false));
    }

    #[test]
    fn load_local_sites_skips_disabled() {
        let dir = std::env::temp_dir().join("sh_savept_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sites2.yaml");
        std::fs::write(
            &p,
            "sites:\n  - name: A\n    url: https://a.org\n  - name: B\n    url: https://b.org\n    enabled: false\n",
        )
        .unwrap();
        let all = load_local_sites(p.to_str().unwrap(), false).unwrap();
        assert_eq!(all.len(), 2);
        let enabled = load_local_sites(p.to_str().unwrap(), true).unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "A");
    }

    #[test]
    fn parse_enabled_line_variants() {
        assert_eq!(
            parse_enabled_line("  enabled: true"),
            Some(("  ".into(), "true".into()))
        );
        assert_eq!(parse_enabled_line("    enabled:true"), None); // 冒号后无空格
        assert_eq!(parse_enabled_line("  enabled_x: 1"), None);
        assert_eq!(parse_enabled_line("  enable: true"), None);
    }
}
