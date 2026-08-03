//! 解析 SolidStart 服务端渲染的 hydration 数据和翻页响应。
//!
//! OpenCode 没有公开用量 API。官方控制台把查询结果序列化为 HTML 中的
//! `$R[...]` 赋值，并通过带帧头的 Seroval 响应完成翻页。本模块只支持这
//! 两个页面产生的纯数据子集；遇到未解析 JavaScript 引用时直接拒绝，绝不
//! 执行上游脚本。

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use scraper::{Html, Selector};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    error::ServiceError,
    model::{
        FIVE_HOUR_QUOTA_USD, MONTHLY_QUOTA_USD, OFFICIAL_PAGE_SIZE, RecordsPage, RequestCost,
        TokenUsage, UsageRecord, UsageSummary, UsageWindow, WEEKLY_QUOTA_USD,
        format_microcents_as_usd, normalize_plan,
    },
};

const GO_QUERY: &str = "lite.subscription.get";
const USER_EMAIL_QUERY: &str = "userEmail";
const USAGE_QUERY: &str = "usage.list";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGoSubscription {
    mine: bool,
    use_balance: bool,
    region: Vec<String>,
    rolling_usage: RawUsageWindow,
    weekly_usage: RawUsageWindow,
    monthly_usage: RawUsageWindow,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUsageWindow {
    status: String,
    reset_in_sec: i64,
    usage_percent: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUsageRecord {
    id: String,
    #[serde(rename = "workspaceID")]
    workspace_id: String,
    time_created: DateTime<Utc>,
    model: String,
    provider: String,
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    reasoning_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    #[serde(default)]
    cache_write_5m_tokens: Option<u64>,
    #[serde(default)]
    cache_write_1h_tokens: Option<u64>,
    cost: u64,
    #[serde(default)]
    #[serde(rename = "keyID")]
    key_id: Option<String>,
    #[serde(default)]
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    #[serde(default)]
    enrichment: Option<RawEnrichment>,
}

#[derive(Debug, Deserialize)]
struct RawEnrichment {
    plan: String,
}

/// 从官网 Usage 首屏提取出的记录页和当前前端资源列表。
pub struct ParsedUsagePage {
    /// 官网第 0 页请求记录。
    pub page: RecordsPage,
    /// 页面声明的 JavaScript modulepreload 和入口资源。
    pub module_assets: Vec<String>,
}

/// 解析官方 Go 控制台页面渲染的三个用量窗口。
pub fn parse_go_summary(
    html: &str,
    fetched_at: DateTime<Utc>,
) -> Result<UsageSummary, ServiceError> {
    let raw: Option<RawGoSubscription> = parse_hydration_query(html, GO_QUERY)?;
    let raw = raw.ok_or(ServiceError::GoSubscriptionMissing)?;

    Ok(UsageSummary {
        owned_by_current_user: raw.mine,
        use_balance_after_limit: raw.use_balance,
        provider_regions: raw.region,
        five_hours: to_window(
            raw.rolling_usage,
            "rolling_5_hours",
            FIVE_HOUR_QUOTA_USD,
            fetched_at,
        ),
        // OpenCode 将其称为每周限额，并按官网周边界重置，而不是滚动 168 小时。
        seven_days: to_window(
            raw.weekly_usage,
            "calendar_week",
            WEEKLY_QUOTA_USD,
            fetched_at,
        ),
        // 官网按订阅纪念日对齐该窗口，不保证从自然月第一天开始。
        one_month: to_window(
            raw.monthly_usage,
            "subscription_month",
            MONTHLY_QUOTA_USD,
            fetched_at,
        ),
        fetched_at,
    })
}

/// 从工作区页面解析当前登录账号对应的邮箱。
pub fn parse_account_email(html: &str) -> Result<String, ServiceError> {
    // 官网把 userEmail 放在最早的 bootstrap 脚本中。直接基于原始 HTML
    // 定位槽位，避免 HTML 重序列化改变这段长脚本中的引用位置。
    let raw: String = parse_hydration_query_source(html, USER_EMAIL_QUERY)?;
    let email = raw.trim();
    let valid = !email.is_empty()
        && email.len() <= 320
        && email.contains('@')
        && !email.chars().any(char::is_whitespace);
    if !valid {
        return Err(ServiceError::UpstreamFormatChanged {
            area: USER_EMAIL_QUERY,
            detail: "hydration query did not contain a valid email".to_owned(),
        });
    }
    Ok(email.to_owned())
}

/// 解析 Usage 页面第 0 页，并收集当前部署使用的 JavaScript 资源。
pub fn parse_usage_page(
    html: &str,
    fetched_at: DateTime<Utc>,
) -> Result<ParsedUsagePage, ServiceError> {
    let records: Vec<RawUsageRecord> = parse_hydration_query(html, USAGE_QUERY)?;
    Ok(ParsedUsagePage {
        page: records_page(0, records, fetched_at)?,
        module_assets: module_asset_paths(html),
    })
}

/// 解析官网上一页/下一页按钮所调用 server-function 返回的非零页。
pub fn parse_usage_rpc_page(
    body: &[u8],
    page: u32,
    fetched_at: DateTime<Utc>,
) -> Result<RecordsPage, ServiceError> {
    if let Ok(records) = serde_json::from_slice::<Vec<RawUsageRecord>>(body) {
        return records_page(page, records, fetched_at);
    }

    let payload = first_seroval_frame(body)?;
    let payload = std::str::from_utf8(payload).map_err(|_| ServiceError::InvalidUtf8)?;
    let marker = "=>$R[0]=";
    let marker_position = payload.find(marker).ok_or_else(|| ServiceError::Rpc {
        detail: "Seroval 响应中缺少根值".to_owned(),
    })?;
    let expression_start = marker_position + marker.len();
    let expression = extract_expression(payload, expression_start, "records rpc")?;
    let records: Vec<RawUsageRecord> = parse_js_value(expression, "records rpc")?;
    records_page(page, records, fetched_at)
}

/// 查找距离 `usage.list` 查询定义最近的 server-function 哈希。
///
/// 这里查找内容哈希而不是压缩后的变量名，可适应 OpenCode 发布时常见的
/// bundle 变量重命名。
pub fn usage_server_function_id(javascript: &str) -> Option<String> {
    let query_position = javascript
        .rfind("\"usage.list\"")
        .or_else(|| javascript.rfind("'usage.list'"))?;
    let search_start = query_position.saturating_sub(4_096);
    last_quoted_hash(&javascript[search_start..query_position])
}

fn to_window(
    raw: RawUsageWindow,
    cycle: &'static str,
    quota_usd: u32,
    fetched_at: DateTime<Utc>,
) -> UsageWindow {
    UsageWindow::new(
        raw.status,
        cycle,
        quota_usd,
        raw.usage_percent,
        raw.reset_in_sec,
        fetched_at,
    )
}

fn records_page(
    page: u32,
    raw_records: Vec<RawUsageRecord>,
    fetched_at: DateTime<Utc>,
) -> Result<RecordsPage, ServiceError> {
    if raw_records.len() > OFFICIAL_PAGE_SIZE {
        return Err(ServiceError::UpstreamFormatChanged {
            area: USAGE_QUERY,
            detail: format!(
                "upstream returned {} records, exceeding the official page size {OFFICIAL_PAGE_SIZE}",
                raw_records.len()
            ),
        });
    }
    let has_next = raw_records.len() == OFFICIAL_PAGE_SIZE;
    Ok(RecordsPage {
        page,
        page_size: OFFICIAL_PAGE_SIZE,
        has_previous: page > 0,
        has_next,
        records: raw_records.into_iter().map(normalize_record).collect(),
        fetched_at,
    })
}

fn normalize_record(raw: RawUsageRecord) -> UsageRecord {
    let cache_read = raw.cache_read_tokens.unwrap_or_default();
    let cache_write_5m = raw.cache_write_5m_tokens.unwrap_or_default();
    let cache_write_1h = raw.cache_write_1h_tokens.unwrap_or_default();
    let input_total = raw
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_write_5m)
        .saturating_add(cache_write_1h);

    UsageRecord {
        id: raw.id,
        workspace_id: raw.workspace_id,
        created_at: raw.time_created,
        model: raw.model,
        provider: raw.provider,
        plan: normalize_plan(raw.enrichment.as_ref().map(|value| value.plan.as_str())),
        tokens: TokenUsage {
            input: raw.input_tokens,
            cache_read,
            cache_write_5m,
            cache_write_1h,
            input_total,
            output: raw.output_tokens,
            reasoning: raw.reasoning_tokens.unwrap_or_default(),
        },
        cost: RequestCost {
            microcents: raw.cost,
            usd: format_microcents_as_usd(raw.cost),
        },
        key_id: non_empty(raw.key_id),
        session_id: non_empty(raw.session_id),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn parse_hydration_query<T: DeserializeOwned>(
    html: &str,
    query_name: &'static str,
) -> Result<T, ServiceError> {
    let scripts = inline_script_source(html)?;
    parse_hydration_query_source(&scripts, query_name)
}

fn parse_hydration_query_source<T: DeserializeOwned>(
    source: &str,
    query_name: &'static str,
) -> Result<T, ServiceError> {
    let marker = format!("_$HY.r[\"{query_name}");
    let registration_start =
        source
            .find(&marker)
            .ok_or_else(|| ServiceError::UpstreamFormatChanged {
                area: query_name,
                detail: "hydration query registration not found".to_owned(),
            })?;
    let registration_end = source[registration_start..]
        .find(';')
        .map(|offset| registration_start + offset)
        .ok_or_else(|| ServiceError::UpstreamFormatChanged {
            area: query_name,
            detail: "unterminated hydration query registration".to_owned(),
        })?;
    let target = r_indices(&source[registration_start..registration_end])
        .last()
        .copied()
        .ok_or_else(|| ServiceError::UpstreamFormatChanged {
            area: query_name,
            detail: "hydration result slot not found".to_owned(),
        })?;
    let expression_start = find_resolution_expression(source, registration_end, target)
        .ok_or_else(|| ServiceError::UpstreamFormatChanged {
            area: query_name,
            detail: "hydration result resolution not found".to_owned(),
        })?;
    let expression = extract_expression(source, expression_start, query_name)?;
    parse_js_value(expression, query_name)
}

fn inline_script_source(html: &str) -> Result<String, ServiceError> {
    let document = Html::parse_document(html);
    let selector =
        Selector::parse("script").map_err(|error| ServiceError::UpstreamFormatChanged {
            area: "html",
            detail: error.to_string(),
        })?;
    let mut source = String::new();
    for script in document.select(&selector) {
        source.push_str(&script.inner_html());
        source.push(';');
    }
    Ok(source)
}

/// 提取 HTML 中声明的 JavaScript 模块资源路径。
pub fn module_asset_paths(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let link_selector = Selector::parse("link[href]").expect("static selector is valid");
    let script_selector = Selector::parse("script[src]").expect("static selector is valid");
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for link in document.select(&link_selector) {
        let rel = link.value().attr("rel").unwrap_or_default();
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if rel
            .split_ascii_whitespace()
            .any(|part| part == "modulepreload")
            && href.contains(".js")
            && seen.insert(href.to_owned())
        {
            result.push(href.to_owned());
        }
    }
    for script in document.select(&script_selector) {
        let Some(src) = script.value().attr("src") else {
            continue;
        };
        if src.contains(".js") && seen.insert(src.to_owned()) {
            result.push(src.to_owned());
        }
    }
    result
}

fn r_indices(input: &str) -> Vec<usize> {
    let bytes = input.as_bytes();
    let mut result = Vec::new();
    let mut index = 0;
    while index + 3 < bytes.len() {
        if !bytes[index..].starts_with(b"$R[") {
            index += 1;
            continue;
        }
        let mut end = index + 3;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > index + 3
            && end < bytes.len()
            && bytes[end] == b']'
            && let Ok(value) = input[index + 3..end].parse()
        {
            result.push(value);
        }
        index = end.saturating_add(1);
    }
    result
}

fn find_resolution_expression(source: &str, start: usize, target: usize) -> Option<usize> {
    let needle = format!("$R[{target}]");
    let mut offset = start;
    while let Some(relative) = source[offset..].find(&needle) {
        let position = offset + relative;
        let previous = source[..position]
            .bytes()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace());
        let mut after = position + needle.len();
        while source
            .as_bytes()
            .get(after)
            .is_some_and(u8::is_ascii_whitespace)
        {
            after += 1;
        }
        if previous == Some(b'(') && source.as_bytes().get(after) == Some(&b',') {
            return Some(after + 1);
        }
        offset = position + needle.len();
    }
    None
}

fn extract_expression<'a>(
    source: &'a str,
    start: usize,
    area: &'static str,
) -> Result<&'a str, ServiceError> {
    let bytes = source.as_bytes();
    let mut index = start;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    let expression_start = index;
    let mut braces = 0_i32;
    let mut brackets = 0_i32;
    let mut parentheses = 0_i32;
    let mut quote = None;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }

        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'(' => parentheses += 1,
            b')' if braces == 0 && brackets == 0 && parentheses == 0 => {
                return Ok(source[expression_start..index].trim());
            }
            b')' => parentheses -= 1,
            _ => {}
        }
        if braces < 0 || brackets < 0 || parentheses < 0 {
            break;
        }
        index += 1;
    }

    Err(ServiceError::UpstreamFormatChanged {
        area,
        detail: "unterminated serialized expression".to_owned(),
    })
}

fn parse_js_value<T: DeserializeOwned>(
    expression: &str,
    area: &'static str,
) -> Result<T, ServiceError> {
    let json = normalize_js_value(expression, area)?;
    serde_json::from_str(&json).map_err(|error| ServiceError::UpstreamFormatChanged {
        area,
        detail: format!("serialized value is incompatible: {error}"),
    })
}

/// 将记录页使用的、不可执行的 Seroval 数据子集转换为 JSON。
fn normalize_js_value(input: &str, area: &'static str) -> Result<String, ServiceError> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if matches!(bytes[index], b'\'' | b'"') {
            index = copy_js_string(bytes, index, &mut output, area)?;
            continue;
        }

        if bytes[index..].starts_with(b"$R[") {
            let mut end = index + 3;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == index + 3 || bytes.get(end) != Some(&b']') {
                return format_error(area, "malformed $R reference");
            }
            end += 1;
            while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                end += 1;
            }
            if bytes.get(end) != Some(&b'=') {
                return format_error(area, "unresolved $R reference refused");
            }
            index = end + 1;
            continue;
        }

        if bytes[index..].starts_with(b"new Date(") {
            index += b"new Date(".len();
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if !matches!(bytes.get(index), Some(b'\'' | b'"')) {
                return format_error(area, "Date constructor does not contain a string");
            }
            index = copy_js_string(bytes, index, &mut output, area)?;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if bytes.get(index) != Some(&b')') {
                return format_error(area, "unterminated Date constructor");
            }
            index += 1;
            continue;
        }

        if bytes[index..].starts_with(b"!0") {
            output.extend_from_slice(b"true");
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"!1") {
            output.extend_from_slice(b"false");
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"undefined") {
            output.extend_from_slice(b"null");
            index += b"undefined".len();
            continue;
        }
        if bytes[index..].starts_with(b"void 0") {
            output.extend_from_slice(b"null");
            index += b"void 0".len();
            continue;
        }

        if is_identifier_start(bytes[index]) {
            let mut end = index + 1;
            while end < bytes.len() && is_identifier_continue(bytes[end]) {
                end += 1;
            }
            let mut colon = end;
            while bytes.get(colon).is_some_and(u8::is_ascii_whitespace) {
                colon += 1;
            }
            if bytes.get(colon) == Some(&b':') {
                output.push(b'"');
                output.extend_from_slice(&bytes[index..end]);
                output.extend_from_slice(b"\":");
                index = colon + 1;
            } else {
                output.extend_from_slice(&bytes[index..end]);
                index = end;
            }
            continue;
        }

        output.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(output).map_err(|_| ServiceError::InvalidUtf8)
}

fn copy_js_string(
    input: &[u8],
    start: usize,
    output: &mut Vec<u8>,
    area: &'static str,
) -> Result<usize, ServiceError> {
    let quote = input[start];
    output.push(b'"');
    let mut index = start + 1;
    while index < input.len() {
        let byte = input[index];
        if byte == quote {
            output.push(b'"');
            return Ok(index + 1);
        }
        if byte == b'"' && quote == b'\'' {
            output.extend_from_slice(b"\\\"");
            index += 1;
            continue;
        }
        if byte != b'\\' {
            output.push(byte);
            index += 1;
            continue;
        }

        let Some(&escaped) = input.get(index + 1) else {
            return format_error(area, "unterminated string escape");
        };
        match escaped {
            b'x' => {
                let digits = input.get(index + 2..index + 4).ok_or_else(|| {
                    ServiceError::UpstreamFormatChanged {
                        area,
                        detail: "short hexadecimal string escape".to_owned(),
                    }
                })?;
                if !digits.iter().all(u8::is_ascii_hexdigit) {
                    return format_error(area, "invalid hexadecimal string escape");
                }
                output.extend_from_slice(b"\\u00");
                output.extend_from_slice(digits);
                index += 4;
            }
            b'\'' if quote == b'\'' => {
                output.push(b'\'');
                index += 2;
            }
            b'v' => {
                output.extend_from_slice(b"\\u000b");
                index += 2;
            }
            b'0' => {
                output.extend_from_slice(b"\\u0000");
                index += 2;
            }
            _ => {
                output.push(b'\\');
                output.push(escaped);
                index += 2;
            }
        }
    }
    format_error(area, "unterminated string")
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn first_seroval_frame(body: &[u8]) -> Result<&[u8], ServiceError> {
    if body.len() < 12 || body.get(1..3) != Some(b"0x") {
        return Ok(body);
    }
    let header = std::str::from_utf8(&body[3..11]).map_err(|_| ServiceError::InvalidUtf8)?;
    let length = usize::from_str_radix(header, 16).map_err(|_| ServiceError::Rpc {
        detail: "Seroval frame length is invalid".to_owned(),
    })?;
    let end = 12_usize
        .checked_add(length)
        .ok_or_else(|| ServiceError::Rpc {
            detail: "Seroval frame length overflow".to_owned(),
        })?;
    body.get(12..end).ok_or_else(|| ServiceError::Rpc {
        detail: "Seroval frame is truncated".to_owned(),
    })
}

fn last_quoted_hash(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut result = None;
    while index < bytes.len() {
        let quote = bytes[index];
        if !matches!(quote, b'\'' | b'"') {
            index += 1;
            continue;
        }
        let start = index + 1;
        index = start;
        let mut escaped = false;
        while index < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == quote {
                break;
            }
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let value = &input[start..index];
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            result = Some(value.to_owned());
        }
        index += 1;
    }
    result
}

fn format_error<T>(area: &'static str, detail: &str) -> Result<T, ServiceError> {
    Err(ServiceError::UpstreamFormatChanged {
        area,
        detail: detail.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_HTML: &str = include_str!("../tests/fixtures/go.html");
    const USAGE_HTML: &str = include_str!("../tests/fixtures/usage.html");

    #[test]
    fn parses_go_hydration_payload() {
        let now = "2026-08-02T07:00:00Z".parse().unwrap();
        let summary = parse_go_summary(GO_HTML, now).unwrap();
        assert_eq!(summary.five_hours.used_percent, 42);
        assert_eq!(summary.seven_days.quota_usd, 30);
        assert_eq!(summary.one_month.resets_in_seconds, 300);
    }

    #[test]
    fn parses_account_email_hydration_payload() {
        let html = r#"<script>self.$R=[];_$HY.r["userEmail[\"wrk_test\"]"]=$R[0]=$R[2]($R[1]={p:0});$R[3]($R[1],"member@example.com");</script>"#;
        assert_eq!(parse_account_email(html).unwrap(), "member@example.com");
    }

    #[test]
    fn parses_usage_records_and_cache_breakdown() {
        let now = "2026-08-02T07:00:00Z".parse().unwrap();
        let parsed = parse_usage_page(USAGE_HTML, now).unwrap();
        assert_eq!(parsed.page.records.len(), 2);
        assert_eq!(parsed.page.records[0].tokens.input_total, 136);
        assert_eq!(parsed.page.records[0].plan, "go");
        assert_eq!(
            parsed.module_assets.last().unwrap(),
            "/assets/usage-route.js"
        );
    }

    #[test]
    fn parses_empty_official_page() {
        let html = r#"<script>self.$R=[];_$HY.r["usage.list[\"wrk_test\",0]"]=$R[1]=$R[2]($R[3]={p:0});$R[4]($R[3],$R[5]=[]);</script>"#;
        let now = Utc::now();
        let parsed = parse_usage_page(html, now).unwrap();
        assert!(parsed.page.records.is_empty());
        assert!(!parsed.page.has_next);
    }

    #[test]
    fn discovers_function_id_near_query_name() {
        let hash = "bfd684bfc2e4eed05cd0b518f5e4eafd3f3376e3938abb9e536e7c03df831e5c";
        let js = format!(
            "const getUsage=createServerReference(\"{hash}\");const q=query(getUsage,\"usage.list\");"
        );
        assert_eq!(usage_server_function_id(&js).as_deref(), Some(hash));
    }

    #[test]
    fn parses_framed_rpc_response() {
        let payload = r#"((self.$R=self.$R||{})["server-fn:1"]=[],($R=>$R[0]=[$R[1]={id:"usg_1",workspaceID:"wrk_test",timeCreated:$R[2]=new Date("2026-08-02T06:00:00Z"),model:"m",provider:"p",inputTokens:1,outputTokens:2,reasoningTokens:0,cacheReadTokens:3,cacheWrite5mTokens:null,cacheWrite1hTokens:null,cost:4,keyID:null,sessionID:"",enrichment:$R[3]={plan:"lite"}}])($R["server-fn:1"]))"#;
        let body = format!(";0x{:08x};{payload}", payload.len());
        let page = parse_usage_rpc_page(body.as_bytes(), 1, Utc::now()).unwrap();
        assert_eq!(page.page, 1);
        assert_eq!(page.records[0].tokens.cache_read, 3);
    }

    #[test]
    fn rejects_truncated_frames_and_oversized_record_pages() {
        assert!(parse_usage_rpc_page(b";0x00000010;short", 1, Utc::now()).is_err());

        let record = r#"{"id":"usg_1","workspaceID":"wrk_test","timeCreated":"2026-08-02T06:00:00Z","model":"m","provider":"p","inputTokens":1,"outputTokens":2,"reasoningTokens":0,"cacheReadTokens":0,"cacheWrite5mTokens":null,"cacheWrite1hTokens":null,"cost":4,"keyID":null,"sessionID":null,"enrichment":{"plan":"lite"}}"#;
        let body = format!("[{}]", vec![record; OFFICIAL_PAGE_SIZE + 1].join(","));
        let error = parse_usage_rpc_page(body.as_bytes(), 1, Utc::now()).unwrap_err();
        assert!(matches!(
            error,
            ServiceError::UpstreamFormatChanged {
                area: USAGE_QUERY,
                ..
            }
        ));
    }

    #[test]
    #[ignore = "需要通过 OPENCODE_REAL_FIXTURE_DIR 指定手动抓取的官网 HTML"]
    fn parses_captured_official_pages() {
        let directory = std::env::var("OPENCODE_REAL_FIXTURE_DIR").unwrap();
        let go = std::fs::read_to_string(format!("{directory}/go.html")).unwrap();
        let usage = std::fs::read_to_string(format!("{directory}/usage.html")).unwrap();
        let summary = parse_go_summary(&go, Utc::now()).unwrap();
        let email = parse_account_email(&go).unwrap();
        let page = parse_usage_page(&usage, Utc::now()).unwrap();

        assert!(summary.five_hours.used_percent <= 100);
        assert!(email.contains('@'));
        assert_eq!(page.page.records.len(), OFFICIAL_PAGE_SIZE);
        assert!(!page.module_assets.is_empty());
    }
}
