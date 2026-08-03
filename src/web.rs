//! Salvo 路由、JSON 接口和内嵌仪表盘。

use std::{fmt::Write as _, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use salvo::{
    http::cookie::{Cookie, SameSite, time::Duration as CookieDuration},
    prelude::*,
};
use salvo_extra::affix_state;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::ServiceError,
    model::{RecordsPage, UsageRecord, UsageSummary},
    opencode::{AccountInfo, AccountRegistry, SourceUrls},
};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const MAX_PAGE: u32 = 10_000;
const PANEL_COOKIE: &str = "opencode_go_panel";
const PANEL_KEY_HEADER: &str = "x-panel-key";

#[derive(Clone)]
struct PanelAuth {
    key: Option<Arc<str>>,
    session_token: Option<Arc<str>>,
}

impl PanelAuth {
    fn new(key: Option<String>) -> Self {
        let key = key.map(Arc::<str>::from);
        let session_token = key
            .as_deref()
            .map(derive_session_token)
            .map(Arc::<str>::from);
        Self { key, session_token }
    }

    fn required(&self) -> bool {
        self.key.is_some()
    }

    fn verify_key(&self, provided: &str) -> bool {
        self.key
            .as_deref()
            .is_none_or(|expected| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
    }

    fn authenticated(&self, req: &Request) -> bool {
        if !self.required() {
            return true;
        }
        let cookie_valid = req.cookie(PANEL_COOKIE).is_some_and(|cookie| {
            self.session_token.as_deref().is_some_and(|expected| {
                constant_time_eq(cookie.value().as_bytes(), expected.as_bytes())
            })
        });
        let header_valid = req
            .headers()
            .get(PANEL_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|provided| self.verify_key(provided));
        cookie_valid || header_valid
    }
}

/// 创建包含面板鉴权、仪表盘、用量接口和健康检查的 Salvo 路由。
pub fn router(accounts: AccountRegistry, panel_key: Option<String>) -> Router {
    Router::new()
        .hoop(affix_state::inject(accounts))
        .hoop(affix_state::inject(PanelAuth::new(panel_key)))
        .get(index)
        .push(
            Router::with_path("api/v1/auth")
                .get(auth_status)
                .post(panel_login)
                .delete(panel_logout),
        )
        .push(Router::with_path("api/v1/accounts").get(account_list))
        .push(Router::with_path("api/v1/usage").get(usage))
        .push(Router::with_path("api/v1/health").get(health))
}

#[handler]
async fn index(res: &mut Response) {
    no_store(res);
    add_header(
        res,
        "content-security-policy",
        "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
    );
    add_header(res, "x-frame-options", "DENY");
    add_header(res, "referrer-policy", "no-referrer");
    res.render(Text::Html(INDEX_HTML));
}

#[handler]
async fn auth_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    no_store(res);
    let auth = panel_auth(depot);
    res.render(Json(PanelAuthResponse {
        required: auth.required(),
        authenticated: auth.authenticated(req),
    }));
}

#[handler]
async fn panel_login(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    no_store(res);
    let auth = panel_auth(depot).clone();
    if !auth.required() {
        res.render(Json(PanelAuthResponse {
            required: false,
            authenticated: true,
        }));
        return;
    }

    let input = match req
        .parse_json_with_max_size::<PanelLoginRequest>(1_024)
        .await
    {
        Ok(input) => input,
        Err(_) => {
            render_auth_request_error(res);
            return;
        }
    };
    if !auth.verify_key(input.key.trim()) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        render_panel_auth_required(res, "面板 Key 无效。");
        return;
    }

    let token = auth
        .session_token
        .as_deref()
        .expect("启用面板鉴权时必须生成会话令牌");
    let mut cookie = Cookie::build((PANEL_COOKIE.to_owned(), token.to_owned()))
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(request_is_secure(req))
        .path("/");
    if input.remember {
        cookie = cookie.max_age(CookieDuration::days(30));
    }
    res.add_cookie(cookie.build());
    res.render(Json(PanelAuthResponse {
        required: true,
        authenticated: true,
    }));
}

#[handler]
async fn panel_logout(req: &mut Request, res: &mut Response) {
    no_store(res);
    res.remove_cookie_with(
        Cookie::build(PANEL_COOKIE)
            .secure(request_is_secure(req))
            .path("/")
            .build(),
    );
    res.status_code(StatusCode::NO_CONTENT);
}

#[handler]
async fn usage(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    no_store(res);
    if !require_panel_auth(req, depot, res) {
        return;
    }
    let page = match parse_page(req) {
        Ok(page) => page,
        Err(message) => {
            render_bad_request(res, message);
            return;
        }
    };
    let force = match parse_force(req) {
        Ok(force) => force,
        Err(message) => {
            render_bad_request(res, message);
            return;
        }
    };
    let registry = depot
        .get_typed::<AccountRegistry>()
        .expect("AccountRegistry 已由 affix-state 注入")
        .clone();
    let account_id = req.query::<String>("account");
    let Some(account) = registry.get(account_id.as_deref()) else {
        render_account_not_found(res);
        return;
    };
    let service = account.service().clone();

    match service.report(page, force).await {
        Ok(report) => {
            let source = service.source_urls();
            let body = UsageResponse {
                generated_at: Utc::now(),
                account: AccountResponse {
                    id: account.id(),
                    name: account.name(),
                    email: report.account_email.as_deref(),
                },
                workspace_id: service.workspace_id(),
                summary: &report.summary,
                request_history: RequestHistoryResponse::from(report.records.as_ref()),
                cache: CacheResponse {
                    ttl_seconds: service.cache_ttl_seconds(),
                    summary_hit: report.summary_cache_hit,
                    records_hit: report.records_cache_hit,
                },
                source: SourceResponse::from(source),
            };
            res.render(Json(body));
        }
        Err(error) => render_service_error(res, error),
    }
}

#[handler]
async fn account_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    no_store(res);
    if !require_panel_auth(req, depot, res) {
        return;
    }
    let registry = depot
        .get_typed::<AccountRegistry>()
        .expect("AccountRegistry 已由 affix-state 注入");
    res.render(Json(AccountsResponse {
        default_account_id: registry.default_id(),
        accounts: registry.list_with_email().await,
    }));
}

#[handler]
async fn health(depot: &mut Depot, res: &mut Response) {
    no_store(res);
    let registry = depot
        .get_typed::<AccountRegistry>()
        .expect("AccountRegistry 已由 affix-state 注入");
    res.render(Json(HealthResponse {
        status: "ok",
        checked_at: Utc::now(),
        account_count: registry.account_count(),
        panel_auth_required: panel_auth(depot).required(),
    }));
}

fn panel_auth(depot: &Depot) -> &PanelAuth {
    depot
        .get_typed::<PanelAuth>()
        .expect("PanelAuth 已由 affix-state 注入")
}

fn require_panel_auth(req: &Request, depot: &Depot, res: &mut Response) -> bool {
    if panel_auth(depot).authenticated(req) {
        return true;
    }
    render_panel_auth_required(res, "请输入正确的面板 Key 后再访问。");
    false
}

fn request_is_secure(req: &Request) -> bool {
    req.uri().scheme_str() == Some("https")
        || req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
}

fn derive_session_token(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"opencode-go-usage panel session\0");
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut token = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut token, "{byte:02x}").expect("写入 String 不会失败");
    }
    token
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for position in 0..length {
        let left = left.get(position).copied().unwrap_or_default();
        let right = right.get(position).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn parse_page(req: &Request) -> Result<u32, String> {
    let Some(raw) = req.query::<String>("page") else {
        return Ok(0);
    };
    let page = raw
        .parse::<u32>()
        .map_err(|_| "page 必须是从 0 开始的整数".to_owned())?;
    if page > MAX_PAGE {
        return Err(format!("page 不能大于 {MAX_PAGE}"));
    }
    Ok(page)
}

fn parse_force(req: &Request) -> Result<bool, String> {
    let Some(raw) = req.query::<String>("refresh") else {
        return Ok(false);
    };
    match raw.as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err("refresh 仅支持 true、false、1、0".to_owned()),
    }
}

fn render_bad_request(res: &mut Response, message: String) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(ErrorEnvelope {
        error: ErrorResponse {
            code: "invalid_query",
            message: &message,
            hint: "示例：/api/v1/usage?account=default&page=0&refresh=false",
        },
    }));
}

fn render_auth_request_error(res: &mut Response) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(ErrorEnvelope {
        error: ErrorResponse {
            code: "invalid_auth_request",
            message: "登录请求格式无效。",
            hint: "提交 JSON：{\"key\":\"你的面板 Key\"}。",
        },
    }));
}

fn render_panel_auth_required(res: &mut Response, message: &'static str) {
    res.status_code(StatusCode::UNAUTHORIZED);
    res.render(Json(ErrorEnvelope {
        error: ErrorResponse {
            code: "panel_authentication_required",
            message,
            hint: "在仪表盘输入 server.panel_key，或通过 X-Panel-Key 请求头访问 API。",
        },
    }));
}

fn render_account_not_found(res: &mut Response) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(ErrorEnvelope {
        error: ErrorResponse {
            code: "account_not_found",
            message: "找不到指定账号。",
            hint: "先请求 /api/v1/accounts 获取可用账号 ID。",
        },
    }));
}

fn render_service_error(res: &mut Response, error: ServiceError) {
    tracing::warn!(code = error.code(), %error, "用量查询失败");
    let message = error.public_message();
    let hint = error.hint();
    res.status_code(error.status_code());
    res.render(Json(ErrorEnvelope {
        error: ErrorResponse {
            code: error.code(),
            message,
            hint,
        },
    }));
}

fn no_store(res: &mut Response) {
    add_header(res, "cache-control", "no-store, max-age=0");
    add_header(res, "pragma", "no-cache");
    add_header(res, "x-content-type-options", "nosniff");
}

fn add_header(res: &mut Response, name: &'static str, value: &'static str) {
    res.add_header(name, value, true)
        .expect("静态响应头必须有效");
}

#[derive(Deserialize)]
struct PanelLoginRequest {
    key: String,
    #[serde(default)]
    remember: bool,
}

#[derive(Serialize)]
struct PanelAuthResponse {
    required: bool,
    authenticated: bool,
}

#[derive(Serialize)]
struct UsageResponse<'a> {
    generated_at: DateTime<Utc>,
    account: AccountResponse<'a>,
    workspace_id: &'a str,
    summary: &'a UsageSummary,
    request_history: RequestHistoryResponse<'a>,
    cache: CacheResponse,
    source: SourceResponse,
}

#[derive(Serialize)]
struct AccountResponse<'a> {
    id: &'a str,
    name: &'a str,
    email: Option<&'a str>,
}

#[derive(Serialize)]
struct AccountsResponse<'a> {
    default_account_id: &'a str,
    accounts: Vec<AccountInfo>,
}

#[derive(Serialize)]
struct RequestHistoryResponse<'a> {
    page: u32,
    page_size: usize,
    returned: usize,
    has_previous: bool,
    has_next: bool,
    fetched_at: DateTime<Utc>,
    records: &'a [UsageRecord],
}

impl<'a> From<&'a RecordsPage> for RequestHistoryResponse<'a> {
    fn from(page: &'a RecordsPage) -> Self {
        Self {
            page: page.page,
            page_size: page.page_size,
            returned: page.records.len(),
            has_previous: page.has_previous,
            has_next: page.has_next,
            fetched_at: page.fetched_at,
            records: &page.records,
        }
    }
}

#[derive(Serialize)]
struct CacheResponse {
    ttl_seconds: u64,
    summary_hit: bool,
    records_hit: bool,
}

#[derive(Serialize)]
struct SourceResponse {
    go_page: String,
    usage_page: String,
    documentation: &'static str,
    transport: &'static str,
}

impl From<SourceUrls> for SourceResponse {
    fn from(source: SourceUrls) -> Self {
        Self {
            go_page: source.go,
            usage_page: source.usage,
            documentation: source.documentation,
            transport: "服务端渲染 HTML + 官网 Usage 页面 server-function",
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    checked_at: DateTime<Utc>,
    account_count: usize,
    panel_auth_required: bool,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorResponse<'a>,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    code: &'a str,
    message: &'a str,
    hint: &'a str,
}

#[cfg(test)]
mod tests {
    use reqwest::{Url, header::HeaderValue as ReqwestHeaderValue};
    use salvo::test::{ResponseExt, TestClient};

    use super::*;
    use crate::config::AccountConfig;

    fn test_registry() -> AccountRegistry {
        let mut cookie = ReqwestHeaderValue::from_static("auth=test");
        cookie.set_sensitive(true);
        AccountRegistry::new(vec![AccountConfig {
            id: "personal".to_owned(),
            name: "个人".to_owned(),
            base_url: Url::parse("http://127.0.0.1:9/").unwrap(),
            cache_ttl: Duration::from_secs(30),
            request_timeout: Duration::from_secs(3),
            workspace_id: "wrk_test".to_owned(),
            cookie,
        }])
        .unwrap()
    }

    #[test]
    fn panel_key_verification_is_exact_and_empty_configuration_disables_auth() {
        let disabled = PanelAuth::new(None);
        assert!(!disabled.required());
        assert!(disabled.verify_key("anything"));

        let enabled = PanelAuth::new(Some("0123456789abcdef".to_owned()));
        assert!(enabled.required());
        assert!(enabled.verify_key("0123456789abcdef"));
        assert!(!enabled.verify_key("0123456789abcdeg"));
        assert!(!enabled.verify_key("0123456789abcdef0"));
    }

    #[test]
    fn session_token_is_stable_and_key_specific() {
        assert_eq!(
            derive_session_token("0123456789abcdef"),
            "750792abe402bcdd2a930af2dc677521b3e45f43928712cf2fdf909f0b52522b"
        );
        assert_ne!(
            derive_session_token("0123456789abcdef"),
            derive_session_token("fedcba9876543210")
        );
    }

    #[tokio::test]
    async fn public_routes_return_health_and_browser_security_headers() {
        let service = Service::new(router(test_registry(), Some("0123456789abcdef".to_owned())));

        let mut health_response = TestClient::get("http://127.0.0.1/api/v1/health")
            .send(&service)
            .await;
        assert_eq!(health_response.status_code, Some(StatusCode::OK));
        assert_eq!(
            health_response
                .headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-store, max-age=0"
        );
        let health_body: serde_json::Value = health_response.take_json().await.unwrap();
        assert_eq!(health_body["status"], "ok");
        assert_eq!(health_body["account_count"], 1);
        assert_eq!(health_body["panel_auth_required"], true);

        let index_response = TestClient::get("http://127.0.0.1/").send(&service).await;
        assert_eq!(index_response.status_code, Some(StatusCode::OK));
        assert_eq!(
            index_response.headers().get("x-frame-options").unwrap(),
            "DENY"
        );
        assert!(
            index_response
                .headers()
                .get("content-security-policy")
                .is_some()
        );
    }

    #[tokio::test]
    async fn protected_routes_enforce_auth_and_validate_before_upstream_calls() {
        const PANEL_KEY: &str = "0123456789abcdef";
        let service = Service::new(router(test_registry(), Some(PANEL_KEY.to_owned())));

        let mut unauthorized = TestClient::get("http://127.0.0.1/api/v1/usage")
            .send(&service)
            .await;
        assert_eq!(unauthorized.status_code, Some(StatusCode::UNAUTHORIZED));
        let body: serde_json::Value = unauthorized.take_json().await.unwrap();
        assert_eq!(body["error"]["code"], "panel_authentication_required");

        let mut invalid = TestClient::get("http://127.0.0.1/api/v1/usage?page=10001")
            .add_header(PANEL_KEY_HEADER, PANEL_KEY, true)
            .send(&service)
            .await;
        assert_eq!(invalid.status_code, Some(StatusCode::BAD_REQUEST));
        let body: serde_json::Value = invalid.take_json().await.unwrap();
        assert_eq!(body["error"]["code"], "invalid_query");

        let mut missing_account =
            TestClient::get("http://127.0.0.1/api/v1/usage?account=missing&page=0")
                .add_header(PANEL_KEY_HEADER, PANEL_KEY, true)
                .send(&service)
                .await;
        assert_eq!(missing_account.status_code, Some(StatusCode::NOT_FOUND));
        let body: serde_json::Value = missing_account.take_json().await.unwrap();
        assert_eq!(body["error"]["code"], "account_not_found");
    }

    #[tokio::test]
    async fn successful_login_sets_a_hardened_session_cookie() {
        const PANEL_KEY: &str = "0123456789abcdef";
        let service = Service::new(router(test_registry(), Some(PANEL_KEY.to_owned())));
        let response = TestClient::post("http://127.0.0.1/api/v1/auth")
            .add_header("x-forwarded-proto", "https", true)
            .raw_json(format!(r#"{{"key":"{PANEL_KEY}","remember":true}}"#))
            .send(&service)
            .await;

        assert_eq!(response.status_code, Some(StatusCode::OK));
        let cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("Max-Age="));
    }
}
