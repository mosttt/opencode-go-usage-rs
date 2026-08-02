//! 单一 JSON 文件配置加载，并对 Cookie 等敏感值做显式保护。

use std::{collections::HashSet, fs, net::SocketAddr, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::{Url, header::HeaderValue};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://opencode.ai";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8787";
const DEFAULT_CACHE_TTL_SECONDS: u64 = 30;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 15;
const MAX_ACCOUNTS: usize = 32;
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Salvo 进程级配置。
///
/// 该类型有意不实现 `Debug`，避免其账号列表间接暴露 Cookie。
pub struct Config {
    /// Salvo 服务监听地址。
    pub bind_addr: SocketAddr,
    /// 面板访问 Key；为空时不启用面板鉴权。
    pub panel_key: Option<String>,
    /// 启动时加载的账号配置，至少包含一个账号。
    pub accounts: Vec<AccountConfig>,
}

/// 单个 OpenCode 账号的独立配置。
///
/// 每个账号会创建独立 HTTP 客户端和缓存，避免 Cookie 或用量数据串用。
pub struct AccountConfig {
    /// 面向 API 的稳定账号 ID。
    pub id: String,
    /// 仪表盘显示名称。
    pub name: String,
    /// OpenCode 控制台基础 URL。
    pub base_url: Url,
    /// 摘要和记录页的内存缓存有效期。
    pub cache_ttl: Duration,
    /// 单次访问官网的请求超时。
    pub request_timeout: Duration,
    /// 需要查询的 OpenCode 工作区 ID。
    pub workspace_id: String,
    /// 标记为敏感值的 OpenCode Cookie 请求头。
    pub cookie: HeaderValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServerConfig,
    accounts: Vec<RawAccount>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawServerConfig {
    bind: Option<String>,
    panel_key: Option<String>,
    cache_ttl_seconds: Option<u64>,
    request_timeout_seconds: Option<u64>,
    base_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccount {
    #[serde(default)]
    id: Option<String>,
    name: String,
    cookie: String,
    workspace_id: String,
    #[serde(default)]
    base_url: Option<String>,
}

impl Config {
    /// 从指定 JSON 文件加载服务和全部账号配置。
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes =
            fs::read(path).with_context(|| format!("无法读取配置文件 {}", path.display()))?;
        if bytes.len() > MAX_CONFIG_BYTES {
            bail!("配置文件不能超过 {MAX_CONFIG_BYTES} 字节");
        }
        let raw: RawConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("配置文件 {} 不是有效 JSON", path.display()))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self> {
        if raw.accounts.is_empty() {
            bail!("配置文件至少需要一个账号");
        }
        if raw.accounts.len() > MAX_ACCOUNTS {
            bail!("账号数量不能超过 {MAX_ACCOUNTS}");
        }

        let bind_addr = raw
            .server
            .bind
            .as_deref()
            .unwrap_or(DEFAULT_BIND_ADDR)
            .parse()
            .context("server.bind 必须是有效的 IP:端口")?;
        let panel_key = normalize_panel_key(raw.server.panel_key.as_deref())?;
        let cache_ttl = Duration::from_secs(bounded_u64(
            "server.cache_ttl_seconds",
            raw.server.cache_ttl_seconds,
            DEFAULT_CACHE_TTL_SECONDS,
            1,
            300,
        )?);
        let request_timeout = Duration::from_secs(bounded_u64(
            "server.request_timeout_seconds",
            raw.server.request_timeout_seconds,
            DEFAULT_REQUEST_TIMEOUT_SECONDS,
            3,
            60,
        )?);
        let default_base_url = raw.server.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);

        let mut ids = HashSet::new();
        let mut accounts = Vec::with_capacity(raw.accounts.len());
        for (index, raw_account) in raw.accounts.into_iter().enumerate() {
            let id = normalize_account_id(raw_account.id.as_deref(), index)?;
            if !ids.insert(id.clone()) {
                bail!("账号 ID {id} 重复");
            }
            let name = normalize_account_name(&raw_account.name, index)?;
            let workspace_id = normalize_workspace_id(&raw_account.workspace_id)?;
            let cookie = normalize_cookie(&raw_account.cookie)
                .with_context(|| format!("账号 {name} 的 Cookie 无效"))?;
            let base_url =
                normalize_base_url(raw_account.base_url.as_deref().unwrap_or(default_base_url))
                    .with_context(|| format!("账号 {name} 的 base_url 无效"))?;

            accounts.push(AccountConfig {
                id,
                name,
                base_url,
                cache_ttl,
                request_timeout,
                workspace_id,
                cookie,
            });
        }

        Ok(Self {
            bind_addr,
            panel_key,
            accounts,
        })
    }
}

fn normalize_account_id(value: Option<&str>, index: usize) -> Result<String> {
    let generated = format!("account-{}", index + 1);
    let value = value.unwrap_or(&generated).trim();
    let valid = !value.is_empty()
        && value.len() <= 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        bail!("账号 ID 仅支持 1 到 40 位 ASCII 字母、数字、下划线和连字符");
    }
    Ok(value.to_owned())
}

fn normalize_account_name(value: &str, index: usize) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 64 {
        bail!("第 {} 个账号的 name 必须是 1 到 64 个字符", index + 1);
    }
    Ok(value.to_owned())
}

fn normalize_panel_key(value: Option<&str>) -> Result<Option<String>> {
    let value = value.unwrap_or_default().trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("server.panel_key 非空时必须是 1 到 256 位可见 ASCII 字符");
    }
    Ok(Some(value.to_owned()))
}

fn normalize_workspace_id(value: &str) -> Result<String> {
    // 允许直接粘贴 wrk_...、/workspace/wrk_.../go 或完整控制台 URL，
    // 降低从地址栏取值时的配置负担。
    let start = value.find("wrk_").unwrap_or(0);
    let candidate = &value[start..];
    let end = candidate
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(candidate.len());
    let candidate = &candidate[..end];
    let valid = candidate.starts_with("wrk_")
        && candidate.len() > 4
        && candidate.len() <= 64
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        bail!("workspace_id 格式无效，应包含地址栏中的 wrk_... 值");
    }
    Ok(candidate.to_owned())
}

fn normalize_cookie(value: &str) -> Result<HeaderValue> {
    let value = value
        .strip_prefix("Cookie:")
        .or_else(|| value.strip_prefix("cookie:"))
        .unwrap_or(value)
        .trim();
    if value.is_empty() {
        bail!("Cookie 不能为空");
    }
    if value.contains(['\r', '\n']) {
        bail!("Cookie 不能包含换行符");
    }
    // 浏览器 Application 面板通常只展示 auth Cookie 的值。若输入中没有
    // Cookie 对分隔符和名称，则自动补全为请求头格式。
    let completed;
    let value = if !value.contains(';') && !value.starts_with("auth=") {
        completed = format!("auth={value}");
        completed.as_str()
    } else {
        value
    };
    let has_auth_cookie = value
        .split(';')
        .map(str::trim)
        .any(|pair| pair.starts_with("auth=") && pair.len() > "auth=".len());
    if !has_auth_cookie {
        bail!("Cookie 必须包含 opencode.ai 工作区页面请求中的 auth=... 值");
    }

    let mut header = HeaderValue::from_str(value).context("不是有效的 HTTP Cookie 请求头值")?;
    header.set_sensitive(true);
    Ok(header)
}

fn normalize_base_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).context("不是有效 URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("必须是 http(s) URL");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("不能包含 query 或 fragment");
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn bounded_u64(name: &str, value: Option<u64>, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = value.unwrap_or(default);
    if !(min..=max).contains(&value) {
        bail!("{name} 必须在 {min}..={max} 之间");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_workspace_id_or_console_url() {
        assert_eq!(normalize_workspace_id("wrk_ABC123").unwrap(), "wrk_ABC123");
        assert_eq!(
            normalize_workspace_id("/workspace/wrk_ABC123/go").unwrap(),
            "wrk_ABC123"
        );
        assert_eq!(
            normalize_workspace_id("https://opencode.ai/workspace/wrk_ABC123/go").unwrap(),
            "wrk_ABC123"
        );
    }

    #[test]
    fn accepts_auth_cookie_value_without_name() {
        let header = normalize_cookie("opaque-session-value").unwrap();
        assert_eq!(header.to_str().unwrap(), "auth=opaque-session-value");
        assert!(header.is_sensitive());
    }

    #[test]
    fn parses_server_and_multiple_accounts() {
        let raw: RawConfig = serde_json::from_str(
            r#"{
                "server": {"bind":"127.0.0.1:9000","panel_key":"0123456789abcdef","cache_ttl_seconds":45},
                "accounts": [
                    {"id":"a","name":"A","cookie":"x","workspace_id":"wrk_A"},
                    {"name":"B","cookie":"y","workspace_id":"/workspace/wrk_B/go"}
                ]
            }"#,
        )
        .unwrap();
        let config = Config::from_raw(raw).unwrap();
        assert_eq!(config.bind_addr.to_string(), "127.0.0.1:9000");
        assert_eq!(config.panel_key.as_deref(), Some("0123456789abcdef"));
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[1].id, "account-2");
        assert_eq!(config.accounts[1].workspace_id, "wrk_B");
        assert_eq!(config.accounts[0].cache_ttl, Duration::from_secs(45));
    }

    #[test]
    fn disables_empty_panel_key_and_validates_header_safe_values() {
        assert_eq!(normalize_panel_key(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_panel_key(Some("short-key")).unwrap().as_deref(),
            Some("short-key")
        );
        assert!(normalize_panel_key(Some("contains space")).is_err());
        let too_long = "x".repeat(257);
        assert!(normalize_panel_key(Some(&too_long)).is_err());
        assert!(normalize_panel_key(Some("0123456789abcdef")).is_ok());
    }
}
