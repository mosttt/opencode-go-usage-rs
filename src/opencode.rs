//! OpenCode 控制台抓取、官网翻页和短时缓存。

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use reqwest::{
    Client, Response, Url,
    header::{
        ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
    },
};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::AccountConfig,
    error::ServiceError,
    model::{RecordsPage, UsageSummary},
    scrape::{
        parse_account_email, parse_go_summary, parse_usage_page, parse_usage_rpc_page,
        usage_server_function_id,
    },
};

const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
const MAX_RPC_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHED_RECORD_PAGES: usize = 64;

/// 多账号注册表，负责按账号 ID 选择完全隔离的用量服务。
#[derive(Clone)]
pub struct AccountRegistry {
    accounts: Arc<Vec<Account>>,
}

/// 一个可供 Web 层选择的账号及其独立用量服务。
#[derive(Clone)]
pub struct Account {
    id: String,
    name: String,
    service: UsageService,
}

/// 不含 Cookie 和工作区信息的公开账号描述。
#[derive(Clone, Serialize)]
pub struct AccountInfo {
    /// 稳定账号 ID，用于 API 的 `account` 查询参数。
    pub id: String,
    /// 仪表盘显示名称。
    pub name: String,
    /// 官网工作区页面返回的当前账号邮箱；读取失败时为空。
    pub email: Option<String>,
}

#[derive(Clone)]
/// 可克隆的 OpenCode 官网用量服务。
pub struct UsageService {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    workspace_id: String,
    cache_ttl: Duration,
    go_url: Url,
    usage_url: Url,
    server_url: Url,
    origin: String,
    cache: RwLock<CacheState>,
    refresh_lock: Mutex<()>,
    server_instance: AtomicU64,
}

#[derive(Default)]
struct CacheState {
    go_page: Option<Cached<GoPageData>>,
    records: HashMap<u32, Cached<RecordsPage>>,
    usage_assets: Vec<Url>,
    server_function: Option<ServerFunction>,
}

struct GoPageData {
    summary: Arc<UsageSummary>,
    account_email: Option<String>,
}

struct Cached<T> {
    stored_at: Instant,
    value: Arc<T>,
}

#[derive(Clone)]
struct ServerFunction {
    id: String,
    asset_url: Url,
}

/// 一次接口查询得到的摘要、记录页和缓存命中信息。
pub struct ServiceReport {
    /// Go 三个周期的用量摘要。
    pub summary: Arc<UsageSummary>,
    /// 官网工作区页面返回的当前账号邮箱。
    pub account_email: Option<String>,
    /// 与官网页码一致的请求记录页。
    pub records: Arc<RecordsPage>,
    /// 摘要是否直接来自有效缓存。
    pub summary_cache_hit: bool,
    /// 当前记录页是否直接来自有效缓存。
    pub records_cache_hit: bool,
}

#[derive(Clone)]
/// 本次抓取使用的官方来源地址。
pub struct SourceUrls {
    /// OpenCode Go 控制台页面。
    pub go: String,
    /// OpenCode Usage 控制台页面。
    pub usage: String,
    /// OpenCode Go 中文官方文档。
    pub documentation: &'static str,
}

impl<T> Cached<T> {
    fn fresh_value(&self, ttl: Duration) -> Option<Arc<T>> {
        (self.stored_at.elapsed() < ttl).then(|| Arc::clone(&self.value))
    }
}

impl AccountRegistry {
    /// 为每个账号创建独立 HTTP 客户端和缓存。
    pub fn new(configs: Vec<AccountConfig>) -> Result<Self> {
        if configs.is_empty() {
            bail!("账号注册表至少需要一个账号");
        }
        let mut accounts = Vec::with_capacity(configs.len());
        for config in configs {
            let id = config.id.clone();
            let name = config.name.clone();
            let service =
                UsageService::new(config).with_context(|| format!("无法初始化账号 {name}"))?;
            accounts.push(Account { id, name, service });
        }
        Ok(Self {
            accounts: Arc::new(accounts),
        })
    }

    /// 返回账号数量。
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// 返回默认账号 ID。
    pub fn default_id(&self) -> &str {
        &self.accounts[0].id
    }

    /// 返回不含任何凭据的账号列表。
    pub fn list(&self) -> Vec<AccountInfo> {
        self.accounts
            .iter()
            .map(|account| AccountInfo {
                id: account.id.clone(),
                name: account.name.clone(),
                email: None,
            })
            .collect()
    }

    /// 并行读取每个账号在官网工作区页面显示的邮箱。
    pub async fn list_with_email(&self) -> Vec<AccountInfo> {
        let fallback = self.list();
        let tasks = self
            .accounts
            .iter()
            .cloned()
            .map(|account| tokio::spawn(async move { account.info_with_email().await }))
            .collect::<Vec<_>>();
        let mut result = Vec::with_capacity(tasks.len());
        for (account, task) in fallback.into_iter().zip(tasks) {
            match task.await {
                Ok(info) => result.push(info),
                Err(error) => {
                    tracing::warn!(account_id = %account.id, %error, "账号邮箱任务异常结束");
                    result.push(account);
                }
            }
        }
        result
    }

    /// 按 ID 查找账号；未传 ID 时返回默认账号。
    pub fn get(&self, id: Option<&str>) -> Option<Account> {
        match id {
            Some(id) => self
                .accounts
                .iter()
                .find(|account| account.id == id)
                .cloned(),
            None => self.accounts.first().cloned(),
        }
    }
}

impl Account {
    /// 返回账号 ID。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 返回账号显示名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回该账号独立的用量服务。
    pub fn service(&self) -> &UsageService {
        &self.service
    }

    async fn info_with_email(self) -> AccountInfo {
        let email = match self.service.account_email().await {
            Ok(email) => email,
            Err(error) => {
                tracing::warn!(
                    account_id = %self.id,
                    code = error.code(),
                    %error,
                    "无法读取账号邮箱"
                );
                None
            }
        };
        AccountInfo {
            id: self.id,
            name: self.name,
            email,
        }
    }
}

impl UsageService {
    /// 根据运行配置创建共享 HTTP 客户端和内存缓存。
    pub fn new(config: AccountConfig) -> Result<Self> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(reqwest::header::COOKIE, config.cookie);
        default_headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
            ),
        );
        default_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        default_headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!(
                "opencode-go-usage/",
                env!("CARGO_PKG_VERSION"),
                " (+local dashboard)"
            )),
        );

        let client = Client::builder()
            .default_headers(default_headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .context("无法创建 OpenCode HTTP 客户端")?;

        let go_url = config
            .base_url
            .join(&format!("workspace/{}/go", config.workspace_id))
            .context("无法构造 OpenCode Go 页面 URL")?;
        let usage_url = config
            .base_url
            .join(&format!("workspace/{}/usage", config.workspace_id))
            .context("无法构造 OpenCode Usage 页面 URL")?;
        let server_url = config
            .base_url
            .join("_server")
            .context("无法构造 OpenCode 网页翻页 URL")?;
        let origin = config.base_url.origin().ascii_serialization();

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                workspace_id: config.workspace_id,
                cache_ttl: config.cache_ttl,
                go_url,
                usage_url,
                server_url,
                origin,
                cache: RwLock::new(CacheState::default()),
                refresh_lock: Mutex::new(()),
                server_instance: AtomicU64::new(0),
            }),
        })
    }

    /// 返回内存缓存有效期秒数。
    pub fn cache_ttl_seconds(&self) -> u64 {
        self.inner.cache_ttl.as_secs()
    }

    /// 返回当前查询的工作区 ID。
    pub fn workspace_id(&self) -> &str {
        &self.inner.workspace_id
    }

    /// 返回官方页面和文档来源。
    pub fn source_urls(&self) -> SourceUrls {
        SourceUrls {
            go: self.inner.go_url.to_string(),
            usage: self.inner.usage_url.to_string(),
            documentation: "https://opencode.ai/docs/zh-cn/go/",
        }
    }

    /// 返回官网工作区页面显示的当前账号邮箱，并复用额度页面缓存。
    pub async fn account_email(&self) -> Result<Option<String>, ServiceError> {
        if let Some(go_page) = self.cached_go_page().await {
            return Ok(go_page.account_email.clone());
        }

        let _refresh = self.inner.refresh_lock.lock().await;
        if let Some(go_page) = self.cached_go_page().await {
            return Ok(go_page.account_email.clone());
        }

        let go_page = Arc::new(self.fetch_go_page().await?);
        let email = go_page.account_email.clone();
        self.store_go_page(go_page).await;
        Ok(email)
    }

    /// 返回摘要和指定官网页。相同页在 TTL 内直接复用，且并发刷新由一个
    /// single-flight 锁合并，避免浏览器自动刷新造成上游请求突发。
    pub async fn report(&self, page: u32, force: bool) -> Result<ServiceReport, ServiceError> {
        let (go_page, records) = self.cached_values(page, force).await;
        if let (Some(go_page), Some(records)) = (go_page, records) {
            return Ok(ServiceReport {
                summary: Arc::clone(&go_page.summary),
                account_email: go_page.account_email.clone(),
                records,
                summary_cache_hit: true,
                records_cache_hit: true,
            });
        }

        let _refresh = self.inner.refresh_lock.lock().await;
        let (mut go_page, mut records) = self.cached_values(page, force).await;
        let summary_cache_hit = go_page.is_some();
        let records_cache_hit = records.is_some();

        match (go_page.is_none(), records.is_none()) {
            (true, true) => {
                let (go_page_result, records_result) =
                    tokio::join!(self.fetch_go_page(), self.fetch_records_page(page));
                go_page = Some(Arc::new(go_page_result?));
                records = Some(Arc::new(records_result?));
            }
            (true, false) => go_page = Some(Arc::new(self.fetch_go_page().await?)),
            (false, true) => records = Some(Arc::new(self.fetch_records_page(page).await?)),
            (false, false) => {}
        }

        let go_page = go_page.expect("go page was fetched or cached");
        let records = records.expect("records were fetched or cached");
        self.store_values(page, Arc::clone(&go_page), Arc::clone(&records))
            .await;

        Ok(ServiceReport {
            summary: Arc::clone(&go_page.summary),
            account_email: go_page.account_email.clone(),
            records,
            summary_cache_hit,
            records_cache_hit,
        })
    }

    async fn cached_values(
        &self,
        page: u32,
        force: bool,
    ) -> (Option<Arc<GoPageData>>, Option<Arc<RecordsPage>>) {
        if force {
            return (None, None);
        }
        let cache = self.inner.cache.read().await;
        let go_page = cache
            .go_page
            .as_ref()
            .and_then(|entry| entry.fresh_value(self.inner.cache_ttl));
        let records = cache
            .records
            .get(&page)
            .and_then(|entry| entry.fresh_value(self.inner.cache_ttl));
        (go_page, records)
    }

    async fn cached_go_page(&self) -> Option<Arc<GoPageData>> {
        self.inner
            .cache
            .read()
            .await
            .go_page
            .as_ref()
            .and_then(|entry| entry.fresh_value(self.inner.cache_ttl))
    }

    async fn store_go_page(&self, go_page: Arc<GoPageData>) {
        self.inner.cache.write().await.go_page = Some(Cached {
            stored_at: Instant::now(),
            value: go_page,
        });
    }

    async fn store_values(&self, page: u32, go_page: Arc<GoPageData>, records: Arc<RecordsPage>) {
        let mut cache = self.inner.cache.write().await;
        cache.go_page = Some(Cached {
            stored_at: Instant::now(),
            value: go_page,
        });
        insert_records_cache(&mut cache, page, records, self.inner.cache_ttl);
    }

    async fn fetch_go_page(&self) -> Result<GoPageData, ServiceError> {
        let html = self.fetch_text(&self.inner.go_url, MAX_HTML_BYTES).await?;
        let summary = Arc::new(parse_go_summary(&html, Utc::now())?);
        let account_email = match parse_account_email(&html) {
            Ok(email) => Some(email),
            Err(error) => {
                tracing::warn!(%error, "无法从官网工作区页面解析账号邮箱");
                None
            }
        };
        Ok(GoPageData {
            summary,
            account_email,
        })
    }

    async fn fetch_records_page(&self, page: u32) -> Result<RecordsPage, ServiceError> {
        if page == 0 {
            return self.fetch_first_records_page().await;
        }

        let function = self.discover_server_function(false).await?;
        match self.fetch_rpc_page(page, &function).await {
            Ok(records) => Ok(records),
            Err(error) if error.is_authentication() => Err(error),
            Err(error) => {
                tracing::warn!(%page, %error, "官网翻页标识可能已更新，重新发现后重试一次");
                let function = self.discover_server_function(true).await?;
                self.fetch_rpc_page(page, &function).await
            }
        }
    }

    async fn fetch_first_records_page(&self) -> Result<RecordsPage, ServiceError> {
        let html = self
            .fetch_text(&self.inner.usage_url, MAX_HTML_BYTES)
            .await?;
        let parsed = parse_usage_page(&html, Utc::now())?;
        let assets = self.resolve_asset_urls(parsed.module_assets)?;
        self.remember_assets(assets).await;
        Ok(parsed.page)
    }

    async fn discover_server_function(&self, force: bool) -> Result<ServerFunction, ServiceError> {
        if !force && let Some(function) = self.inner.cache.read().await.server_function.clone() {
            return Ok(function);
        }

        let mut assets = if force {
            Vec::new()
        } else {
            self.inner.cache.read().await.usage_assets.clone()
        };
        if assets.is_empty() {
            let html = self
                .fetch_text(&self.inner.usage_url, MAX_HTML_BYTES)
                .await?;
            let parsed = parse_usage_page(&html, Utc::now())?;
            assets = self.resolve_asset_urls(parsed.module_assets)?;

            let page = Arc::new(parsed.page);
            let mut cache = self.inner.cache.write().await;
            insert_records_cache(&mut cache, 0, page, self.inner.cache_ttl);
            set_assets(&mut cache, assets.clone());
        }

        // 路由专属 bundle 通常位于 modulepreload 列表末尾，倒序查找可在
        // 正常情况下只下载一个小型 JS 文件。
        for asset_url in assets.iter().rev() {
            let javascript = match self.fetch_text(asset_url, MAX_ASSET_BYTES).await {
                Ok(javascript) => javascript,
                Err(error) if error.is_authentication() => return Err(error),
                Err(error) => {
                    tracing::debug!(url = %asset_url, %error, "跳过无法读取的官网 JS 资源");
                    continue;
                }
            };
            let Some(id) = usage_server_function_id(&javascript) else {
                continue;
            };
            let function = ServerFunction {
                id,
                asset_url: asset_url.clone(),
            };
            self.inner.cache.write().await.server_function = Some(function.clone());
            return Ok(function);
        }

        Err(ServiceError::UpstreamFormatChanged {
            area: "usage pagination",
            detail: "usage.list server function id not found in module assets".to_owned(),
        })
    }

    async fn fetch_rpc_page(
        &self,
        page: u32,
        function: &ServerFunction,
    ) -> Result<RecordsPage, ServiceError> {
        let instance_number = self.inner.server_instance.fetch_add(1, Ordering::Relaxed);
        let instance = format!("server-fn:{instance_number}");
        let body = json!({
            "t": {
                "t": 9,
                "i": 0,
                "l": 2,
                "a": [
                    { "t": 1, "s": self.inner.workspace_id },
                    { "t": 0, "s": page }
                ],
                "o": 0
            },
            "f": 31,
            "m": []
        });

        tracing::debug!(
            %page,
            asset = %function.asset_url,
            "调用官网 Usage 页面自身的翻页通道"
        );
        let response = self
            .inner
            .client
            .post(self.inner.server_url.clone())
            .header("x-server-id", &function.id)
            .header("x-server-instance", &instance)
            .header(ORIGIN, &self.inner.origin)
            .header(REFERER, self.inner.usage_url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;

        if is_authentication_response(&response) {
            return Err(ServiceError::Authentication);
        }
        let status = response.status();
        let server_error = response.headers().contains_key("x-error");
        if !status.is_success() || server_error {
            return Err(ServiceError::Rpc {
                detail: format!("HTTP {} or X-Error from server function", status.as_u16()),
            });
        }
        let bytes = read_response_bytes(response, MAX_RPC_BYTES).await?;
        parse_usage_rpc_page(&bytes, page, Utc::now())
    }

    async fn fetch_text(&self, url: &Url, max_bytes: usize) -> Result<String, ServiceError> {
        let response = self.inner.client.get(url.clone()).send().await?;
        if is_authentication_response(&response) {
            return Err(ServiceError::Authentication);
        }
        if !response.status().is_success() {
            return Err(ServiceError::UpstreamStatus {
                status: response.status().as_u16(),
            });
        }
        let bytes = read_response_bytes(response, max_bytes).await?;
        String::from_utf8(bytes).map_err(|_| ServiceError::InvalidUtf8)
    }

    fn resolve_asset_urls(&self, paths: Vec<String>) -> Result<Vec<Url>, ServiceError> {
        let mut result = Vec::new();
        for path in paths {
            let url = self.inner.usage_url.join(&path).map_err(|error| {
                ServiceError::UpstreamFormatChanged {
                    area: "usage assets",
                    detail: format!("invalid asset URL: {error}"),
                }
            })?;
            if url.origin() == self.inner.usage_url.origin() {
                result.push(url);
            }
        }
        if result.is_empty() {
            return Err(ServiceError::UpstreamFormatChanged {
                area: "usage assets",
                detail: "no same-origin JavaScript module found".to_owned(),
            });
        }
        Ok(result)
    }

    async fn remember_assets(&self, assets: Vec<Url>) {
        let mut cache = self.inner.cache.write().await;
        set_assets(&mut cache, assets);
    }
}

fn insert_records_cache(cache: &mut CacheState, page: u32, value: Arc<RecordsPage>, ttl: Duration) {
    cache
        .records
        .retain(|_, entry| entry.stored_at.elapsed() < ttl);
    if cache.records.len() >= MAX_CACHED_RECORD_PAGES
        && let Some(oldest_page) = cache
            .records
            .iter()
            .min_by_key(|(_, entry)| entry.stored_at)
            .map(|(page, _)| *page)
    {
        cache.records.remove(&oldest_page);
    }
    cache.records.insert(
        page,
        Cached {
            stored_at: Instant::now(),
            value,
        },
    );
}

fn set_assets(cache: &mut CacheState, assets: Vec<Url>) {
    let function_is_current = cache
        .server_function
        .as_ref()
        .is_some_and(|function| assets.contains(&function.asset_url));
    cache.usage_assets = assets;
    if !function_is_current {
        cache.server_function = None;
    }
}

async fn read_response_bytes(
    response: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ServiceError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ServiceError::BodyTooLarge { limit: max_bytes });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > max_bytes {
        return Err(ServiceError::BodyTooLarge { limit: max_bytes });
    }
    Ok(bytes.to_vec())
}

fn is_authentication_response(response: &Response) -> bool {
    if matches!(response.status().as_u16(), 401 | 403) {
        return true;
    }
    if response.status().is_redirection() {
        return response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|location| location.contains("/auth") || location.contains("auth."));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_config(id: &str, name: &str, workspace_id: &str) -> AccountConfig {
        let mut cookie = HeaderValue::from_static("auth=test");
        cookie.set_sensitive(true);
        AccountConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            base_url: Url::parse("https://opencode.ai/").unwrap(),
            cache_ttl: Duration::from_secs(30),
            request_timeout: Duration::from_secs(5),
            workspace_id: workspace_id.to_owned(),
            cookie,
        }
    }

    #[test]
    fn selects_accounts_by_id_without_exposing_credentials() {
        let registry = AccountRegistry::new(vec![
            account_config("personal", "个人", "wrk_A"),
            account_config("work", "工作", "wrk_B"),
        ])
        .unwrap();

        assert_eq!(registry.account_count(), 2);
        assert_eq!(registry.default_id(), "personal");
        let work = registry.get(Some("work")).unwrap();
        assert_eq!(work.name(), "工作");
        assert_eq!(work.service().workspace_id(), "wrk_B");
        assert_eq!(
            registry
                .get(Some("personal"))
                .unwrap()
                .service()
                .workspace_id(),
            "wrk_A"
        );
        assert!(registry.get(Some("missing")).is_none());
        assert_eq!(registry.list()[0].name, "个人");
    }

    #[test]
    fn rejects_empty_account_registry() {
        assert!(AccountRegistry::new(Vec::new()).is_err());
    }
}
