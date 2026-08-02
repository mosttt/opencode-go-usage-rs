//! OpenCode 官网适配层的错误分类及安全的公开错误信息。

use salvo::http::StatusCode;
use thiserror::Error;

/// 官网抓取、解析和翻页过程中可能出现的错误。
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Cookie 已过期、缺失，或官网把请求重定向到登录页。
    #[error("OpenCode 登录态已失效或 Cookie 不完整")]
    Authentication,

    /// 当前工作区没有 Go 订阅数据。
    #[error("当前工作区没有可读取的 OpenCode Go 订阅")]
    GoSubscriptionMissing,

    /// 访问 OpenCode 时出现网络错误。
    #[error("OpenCode 上游请求失败: {0}")]
    Network(#[from] reqwest::Error),

    /// 官网返回非成功 HTTP 状态。
    #[error("OpenCode 上游返回 HTTP {status}")]
    UpstreamStatus {
        /// 上游 HTTP 状态码。
        status: u16,
    },

    /// 官网响应超过服务设置的安全上限。
    #[error("OpenCode 上游响应超过 {limit} 字节")]
    BodyTooLarge {
        /// 允许读取的最大字节数。
        limit: usize,
    },

    /// 官网响应无法按 UTF-8 解码。
    #[error("OpenCode 上游响应不是 UTF-8")]
    InvalidUtf8,

    /// 官网 HTML 或序列化格式与当前解析器不兼容。
    #[error("OpenCode 网页结构发生变化: {area}: {detail}")]
    UpstreamFormatChanged {
        /// 出现变化的解析区域。
        area: &'static str,
        /// 仅用于服务端日志的诊断信息。
        detail: String,
    },

    /// 官网 Usage 页自身的翻页通道调用失败。
    #[error("OpenCode 网页分页通道调用失败: {detail}")]
    Rpc {
        /// 不包含 Cookie 和响应正文的诊断信息。
        detail: String,
    },
}

impl ServiceError {
    /// 判断错误是否由登录态失效引起。
    pub fn is_authentication(&self) -> bool {
        matches!(self, Self::Authentication)
    }

    /// 返回适合 JSON 接口的 HTTP 状态码。
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Authentication => StatusCode::UNAUTHORIZED,
            Self::GoSubscriptionMissing => StatusCode::NOT_FOUND,
            Self::Network(_) => StatusCode::BAD_GATEWAY,
            Self::UpstreamStatus { .. }
            | Self::BodyTooLarge { .. }
            | Self::InvalidUtf8
            | Self::UpstreamFormatChanged { .. }
            | Self::Rpc { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    /// 返回稳定、便于客户端判断的错误代码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Authentication => "opencode_authentication_required",
            Self::GoSubscriptionMissing => "go_subscription_missing",
            Self::Network(_) => "opencode_network_error",
            Self::UpstreamStatus { .. } => "opencode_upstream_status",
            Self::BodyTooLarge { .. } => "opencode_response_too_large",
            Self::InvalidUtf8 => "opencode_invalid_utf8",
            Self::UpstreamFormatChanged { .. } => "opencode_format_changed",
            Self::Rpc { .. } => "opencode_pagination_failed",
        }
    }

    /// 返回不会泄露官网响应内容的用户提示。
    pub fn public_message(&self) -> &'static str {
        match self {
            Self::Authentication => "OpenCode 登录态无效，请更新当前账号的 Cookie。",
            Self::GoSubscriptionMissing => "当前工作区未找到可读取的 OpenCode Go 订阅。",
            Self::Network(_) => "暂时无法连接 OpenCode。",
            Self::UpstreamStatus { .. } => "OpenCode 返回了非预期状态。",
            Self::BodyTooLarge { .. } | Self::InvalidUtf8 => "OpenCode 返回了无法处理的页面。",
            Self::UpstreamFormatChanged { .. } => "OpenCode 网页结构已变化，需要更新解析器。",
            Self::Rpc { .. } => "OpenCode 记录翻页暂时不可用。",
        }
    }

    /// 返回建议的排查步骤。
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Authentication => "重新登录 opencode.ai，并替换当前账号配置中的 Cookie。",
            Self::GoSubscriptionMissing => {
                "确认工作区 ID 正确，且当前账号是该 Go 订阅的工作区成员。"
            }
            Self::UpstreamFormatChanged { .. } | Self::Rpc { .. } => {
                "查看服务日志中的 area 信息，并对照官方控制台最新源码。"
            }
            _ => "稍后重试；若持续失败，请检查网络和 OpenCode 服务状态。",
        }
    }
}
