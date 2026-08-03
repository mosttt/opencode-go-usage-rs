//! 与 OpenCode 内部字段名解耦的稳定接口模型。

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

/// OpenCode Go 官方文档在 2026-08-02 公布的 5 小时额度（美元）。
///
/// 官网只返回整数百分比而不是原始计数，因此接口只同时提供官方额度和
/// 百分比，不伪造带有虚假精度的 `used_usd`。
pub const FIVE_HOUR_QUOTA_USD: u32 = 12;
/// OpenCode Go 官方文档在 2026-08-02 公布的每周额度（美元）。
pub const WEEKLY_QUOTA_USD: u32 = 30;
/// OpenCode Go 官方文档在 2026-08-02 公布的每月额度（美元）。
pub const MONTHLY_QUOTA_USD: u32 = 60;
/// 官网 Usage 页面当前固定使用的每页记录数。
pub const OFFICIAL_PAGE_SIZE: usize = 50;
const MAX_RESET_SECONDS: i64 = 366 * 24 * 60 * 60;

/// Go 订阅的三个用量窗口及订阅设置。
#[derive(Clone, Debug, Serialize)]
pub struct UsageSummary {
    /// 当前登录账号是否为该 Go 订阅的订阅者。
    pub owned_by_current_user: bool,
    /// 达到 Go 限额后是否继续消耗 Zen 余额。
    pub use_balance_after_limit: bool,
    /// 官网为当前工作区启用的模型服务区域。
    pub provider_regions: Vec<String>,
    /// 5 小时滚动用量窗口。
    pub five_hours: UsageWindow,
    /// 官网每周用量窗口；字段名满足“7 天”展示需求。
    pub seven_days: UsageWindow,
    /// 按订阅周期计算的月度用量窗口。
    pub one_month: UsageWindow,
    /// 该摘要从官网抓取完成的时间。
    pub fetched_at: DateTime<Utc>,
}

/// 单个用量周期的百分比、额度和重置时间。
#[derive(Clone, Debug, Serialize)]
pub struct UsageWindow {
    /// 官网状态，例如 `ok` 或 `rate-limited`。
    pub status: String,
    /// 周期语义，例如滚动 5 小时、自然周或订阅月。
    pub cycle: &'static str,
    /// 官方公布的周期额度，单位为美元。
    pub quota_usd: u32,
    /// 官网计算并向下取整后的已用百分比。
    pub used_percent: u8,
    /// 由已用百分比计算的剩余百分比。
    pub remaining_percent: u8,
    /// 距离官网重置时间的秒数。
    pub resets_in_seconds: u64,
    /// 根据抓取时间和官网剩余秒数计算的重置时刻。
    pub resets_at: DateTime<Utc>,
}

impl UsageWindow {
    /// 根据官网序列化结果创建规范化的用量窗口。
    pub fn new(
        status: String,
        cycle: &'static str,
        quota_usd: u32,
        used_percent: u64,
        resets_in_seconds: i64,
        fetched_at: DateTime<Utc>,
    ) -> Self {
        let used_percent = used_percent.min(100) as u8;
        let reset_seconds = resets_in_seconds.clamp(0, MAX_RESET_SECONDS);
        let (resets_at, resets_in_seconds) = fetched_at
            .checked_add_signed(TimeDelta::seconds(reset_seconds))
            .map_or((fetched_at, 0), |resets_at| {
                (resets_at, reset_seconds as u64)
            });
        Self {
            status,
            cycle,
            quota_usd,
            used_percent,
            remaining_percent: 100 - used_percent,
            resets_in_seconds,
            resets_at,
        }
    }
}

/// 与官网页码一一对应的请求记录页。
#[derive(Clone, Debug, Serialize)]
pub struct RecordsPage {
    /// 从 0 开始的官网页码。
    pub page: u32,
    /// 官网固定的每页记录上限。
    pub page_size: usize,
    /// 是否可以返回上一页。
    pub has_previous: bool,
    /// 是否按官网规则显示下一页入口。
    pub has_next: bool,
    /// 当前页请求记录。
    pub records: Vec<UsageRecord>,
    /// 当前页从官网抓取完成的时间。
    pub fetched_at: DateTime<Utc>,
}

/// 单次模型请求的用量记录。
#[derive(Clone, Debug, Serialize)]
pub struct UsageRecord {
    /// OpenCode 用量记录 ID。
    pub id: String,
    /// 记录所属工作区 ID。
    pub workspace_id: String,
    /// 请求创建时间。
    pub created_at: DateTime<Utc>,
    /// 请求使用的模型 ID。
    pub model: String,
    /// OpenCode 内部记录的上游提供方。
    pub provider: String,
    /// 规范化套餐名，例如 `go`、`subscription` 或 `byok`。
    pub plan: String,
    /// Token 总量及缓存明细。
    pub tokens: TokenUsage,
    /// 官网记账费用。
    pub cost: RequestCost,
    /// 发起请求的 API Key ID。
    pub key_id: Option<String>,
    /// OpenCode 会话 ID；官网未提供时为空。
    pub session_id: Option<String>,
}

/// 单次请求的输入、缓存、输出和推理 Token 明细。
#[derive(Clone, Debug, Serialize)]
pub struct TokenUsage {
    /// 未命中缓存的原始输入 Token。
    pub input: u64,
    /// 从提示缓存读取的 Token。
    pub cache_read: u64,
    /// 写入 5 分钟提示缓存的 Token。
    pub cache_write_5m: u64,
    /// 写入 1 小时提示缓存的 Token。
    pub cache_write_1h: u64,
    /// 原始输入与全部缓存读写之和，与官网输入列口径一致。
    pub input_total: u64,
    /// 官网输出 Token。
    pub output: u64,
    /// 官网单独记录的推理 Token。
    pub reasoning: u64,
}

/// 单次请求的精确费用表示。
#[derive(Clone, Debug, Serialize)]
pub struct RequestCost {
    /// OpenCode 原始记账单位；100,000,000 microcents 等于 1 美元。
    pub microcents: u64,
    /// 避免浮点舍入的精确美元十进制字符串。
    pub usd: String,
}

/// 将 OpenCode 的 microcents 整数转换为精确美元字符串。
pub fn format_microcents_as_usd(value: u64) -> String {
    const UNITS_PER_DOLLAR: u64 = 100_000_000;
    let whole = value / UNITS_PER_DOLLAR;
    let fraction = value % UNITS_PER_DOLLAR;
    let mut result = format!("{whole}.{fraction:08}");
    while result.ends_with('0') && !result.ends_with(".0000") {
        result.pop();
    }
    result
}

/// 将官网内部套餐名转换为面向使用者的名称。
pub fn normalize_plan(value: Option<&str>) -> String {
    match value {
        Some("lite") => "go".to_owned(),
        Some("sub") => "subscription".to_owned(),
        Some("byok") => "byok".to_owned(),
        Some(other) => other.to_owned(),
        None => "balance".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_microcents_without_float_rounding() {
        assert_eq!(format_microcents_as_usd(73_998), "0.00073998");
        assert_eq!(format_microcents_as_usd(100_000_000), "1.0000");
        assert_eq!(format_microcents_as_usd(0), "0.0000");
    }

    #[test]
    fn clamps_invalid_usage_window_values_without_panicking() {
        let fetched_at = Utc::now();
        let window = UsageWindow::new(
            "ok".to_owned(),
            "rolling_5_hours",
            FIVE_HOUR_QUOTA_USD,
            u64::MAX,
            i64::MAX,
            fetched_at,
        );

        assert_eq!(window.used_percent, 100);
        assert_eq!(window.remaining_percent, 0);
        assert_eq!(window.resets_in_seconds, MAX_RESET_SECONDS as u64);
        assert_eq!(
            window.resets_at,
            fetched_at + TimeDelta::seconds(MAX_RESET_SECONDS)
        );

        let negative = UsageWindow::new(
            "ok".to_owned(),
            "rolling_5_hours",
            FIVE_HOUR_QUOTA_USD,
            0,
            i64::MIN,
            fetched_at,
        );
        assert_eq!(negative.resets_in_seconds, 0);
        assert_eq!(negative.resets_at, fetched_at);
    }
}
