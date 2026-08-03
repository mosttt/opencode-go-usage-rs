# OpenCode Go 用量查询

这是一个可本地运行或自托管的 Rust + Salvo 服务，通过一个或多个账号的登录 Cookie 读取 OpenCode 官方控制台，提供：

- 5 小时、每周（7d 展示）和订阅月用量百分比
- 官方额度、剩余百分比和重置时间
- 请求使用记录，页码和每页 50 条上限与官网一致
- 输入 Token 展开明细：原始输入、缓存读取、5 分钟缓存写入、1 小时缓存写入
- 输出 Token 和推理 Token 展开明细
- JSON API 和无需构建工具的中文仪表盘
- 多账号切换，每个账号使用独立 Cookie、工作区、连接池和缓存
- 短时内存缓存和并发刷新合并
- 可选面板 Key、30 天持久会话、二进制/systemd 和 Docker 部署

> OpenCode 当前没有公开的 Go 用量 SDK 或稳定 API。该项目是“登录态网页适配器”，不是 OpenCode 官方 SDK。官网结构变化时，服务会返回明确错误，不会静默生成错误数据。

## 设计取舍

| 目标 | 实现 |
| --- | --- |
| 易用性 | 一个进程、一组小型 JSON 接口、一个内嵌页面；所有设置统一放在 `config.json` |
| 低心智负担 | 页码、每页数量、Token 合计方式均跟随官网，不另造统计口径 |
| 高维护性 | 不使用无头浏览器；动态发现当前官网 server-function 哈希；解析失败即显式报错 |
| 高性能 | 每账号复用 reqwest 连接池；摘要和各记录页独立缓存；相同时间的刷新合并为一次上游抓取 |
| 安全性 | Cookie 只存在于服务端；可选面板 Key；禁止缓存响应；本机示例仅监听 `127.0.0.1`；不执行官网 JavaScript |

详细原理见 [`docs/architecture.md`](docs/architecture.md)，生产部署见 [`docs/deployment.md`](docs/deployment.md)。

## 官方依据

本实现于 2026-08-02 对照以下官方页面和源码：

- [OpenCode Go 中文文档](https://opencode.ai/docs/zh-cn/go/)
- [OpenCode Go 官方页面](https://opencode.ai/zh/go)
- [Go 用量窗口源码 `lite-section.tsx`](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/routes/workspace/%5Bid%5D/go/lite-section.tsx)
- [请求记录源码 `usage-section.tsx`](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/routes/workspace/%5Bid%5D/usage/usage-section.tsx)
- [官网记账模型 `billing.sql.ts`](https://github.com/anomalyco/opencode/blob/dev/packages/console/core/src/schema/billing.sql.ts)
- [周期计算源码 `subscription.ts`](https://github.com/anomalyco/opencode/blob/dev/packages/console/core/src/subscription.ts)

官方文档当前公布的 Go 限额为：

| 周期 | 额度 |
| --- | ---: |
| 5 小时 | $12 |
| 每周 | $30 |
| 每月 | $60 |

官网返回的是向下取整后的整数百分比，并不返回原始周期消费值。因此本服务不会用百分比反推一个看似精确的已用美元数。

## 环境要求

- Rust 1.97 或更高版本
- 有效的 OpenCode 登录态
- 已加入目标工作区并可在官网打开 Go 和 Usage 页面
- Docker 部署时需要 Docker Engine 及 Compose v2

## 配置

服务只读取进程当前工作目录中的 `config.json`，不读取 `.env` 或账号环境变量，也不会自动生成含凭据的配置。请按运行方式选择示例：

```bash
# 本机二进制或 cargo run：默认只监听本机
cp config.local.example.json config.json

# Docker Compose：容器内监听 0.0.0.0，宿主机仍只发布到 127.0.0.1
cp config.example.json config.json
```

Windows PowerShell 分别使用 `Copy-Item config.local.example.json config.json` 或 `Copy-Item config.example.json config.json`。未创建该文件就启动会明确报错 `无法读取配置文件 config.json`（Windows 常见后续原因为 `os error 2`）。

`config.json` 同时保存服务设置、面板鉴权和全部账号：

```json
{
  "server": {
    "panel_key": "",
    "bind": "0.0.0.0:8787",
    "cache_ttl_seconds": 30,
    "request_timeout_seconds": 15,
    "base_url": "https://opencode.ai"
  },
  "accounts": [
    {
      "id": "personal",
      "name": "个人账号",
      "cookie": "auth=...",
      "workspace_id": "/workspace/wrk_xxx/go"
    },
    {
      "id": "work",
      "name": "工作账号",
      "cookie": "只填写 auth Cookie 值也可以",
      "workspace_id": "https://opencode.ai/workspace/wrk_yyy/go"
    }
  ]
}
```

`config.example.json` 默认监听 `0.0.0.0:8787`，复制后可直接用于 Docker Compose；`config.local.example.json` 默认监听 `127.0.0.1:8787`，用于本机二进制或 `cargo run`。Compose 只把端口发布到宿主机 `127.0.0.1`，因此容器内监听全部接口不等于直接暴露公网。

`id` 可省略，服务会生成 `account-1`、`account-2`。账号 ID 仅支持 ASCII 字母、数字、下划线和连字符，且必须唯一。账号数量上限为 32。

只使用一个账号时，`accounts` 数组保留一个对象即可。

### 获取工作区 ID

1. 登录 [opencode.ai](https://opencode.ai/auth)。
2. 打开目标工作区的 Go 页面。
3. `workspace_id` 可填写地址中的 `wrk_xxx`、`/workspace/wrk_xxx/go`，也可直接粘贴完整工作区 URL，服务会自动提取。

### 获取 Cookie

1. 在浏览器开发者工具中打开 Network。
2. 刷新 `https://opencode.ai/workspace/<工作区ID>/go`。
3. 选择发往 `opencode.ai` 的文档请求。
4. 在 Request Headers 中取出完整 `Cookie` 值，不要包含 `Cookie:` 前缀也可以，服务会兼容两种写法。
5. 确认取自 `opencode.ai/workspace/...` 请求而不是 `auth.opencode.ai` 登录页。可以填写完整 `auth=...`，也可以只填写 Application 面板中 `auth` Cookie 的值，服务会自动补全名称。

认证 Cookie 是 HttpOnly，通常不能通过 `document.cookie` 得到。不要把 `config.json` 提交到 Git，也不要把 Cookie 或面板 Key 发到聊天、日志或截图中。建议执行 `chmod 600 config.json`。

### 面板 Key

`server.panel_key` 为空字符串时不验证，浏览器会直接显示仪表盘。任意非空、最多 256 位的可见 ASCII 字符都会启用验证；公网部署推荐至少 32 位随机 Key。

可生成高强度 Key：

```bash
openssl rand -hex 32
```

登录成功后服务设置 `HttpOnly`、`SameSite=Strict` Cookie。默认只保持当前浏览器会话；勾选“记住 Key”后将派生会话令牌保持 30 天。应用不会把原始 Key 写入 Cookie 或浏览器本地存储。修改 `server.panel_key` 并重启服务后，旧会话立即失效。

### 服务配置

| JSON 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `server.bind` | `127.0.0.1:8787` | Salvo 监听地址；示例配置为兼容容器而设置成 `0.0.0.0:8787` |
| `server.panel_key` | 空 | 面板 Key；为空不鉴权，公网推荐至少 32 位随机值 |
| `server.cache_ttl_seconds` | `30` | 内存缓存秒数，范围 1 到 300 |
| `server.request_timeout_seconds` | `15` | 官网请求超时，范围 3 到 60 秒 |
| `server.base_url` | `https://opencode.ai` | 所有账号默认官网地址；非回环地址必须使用 HTTPS |
| `accounts[].base_url` | 继承 `server.base_url` | 单个账号覆盖官网地址；HTTP 仅允许 `localhost` 或回环 IP |

## 运行

```bash
cargo run --release
```

打开：

```text
http://127.0.0.1:8787
```

服务启动时会校验配置格式，但不会把 Cookie 或面板 Key 打印到日志中。

二进制、systemd、Docker Compose、Caddy 和 Nginx 的完整步骤见 [`docs/deployment.md`](docs/deployment.md)。

### Docker Compose

编辑好容器版 `config.json` 后可直接使用 GHCR 多架构镜像：

```bash
IMAGE_TAG=0.1.1 docker compose pull
IMAGE_TAG=0.1.1 LOCAL_UID="$(id -u)" LOCAL_GID="$(id -g)" docker compose up -d
```

PowerShell：

```powershell
$env:IMAGE_TAG = "0.1.1"
docker compose pull
docker compose up -d
```

生产环境建议固定明确版本（例如 `0.1.1`）；未设置 `IMAGE_TAG` 时使用会随 `main` 更新的 `latest`。镜像地址为 `ghcr.io/mosttt/opencode-go-usage-rs`，支持 `linux/amd64` 和 `linux/arm64`。

## JSON API

### 面板鉴权

浏览器通过以下接口建立和清除面板会话：

```http
GET    /api/v1/auth
POST   /api/v1/auth
DELETE /api/v1/auth
```

登录请求：

```json
{ "key": "你的面板 Key", "remember": true }
```

`remember` 可省略，默认 `false`；为 `true` 时会话 Cookie 保存 30 天。

脚本或监控程序不需要建立 Cookie 会话，可直接为受保护 API 添加请求头：

```http
X-Panel-Key: 你的面板 Key
```

Key 不支持 URL 查询参数，避免进入访问日志和浏览器历史。`server.panel_key` 为空时，账号和用量 API 不要求 Cookie 或请求头。

### 查询账号列表

```http
GET /api/v1/accounts
```

```json
{
  "default_account_id": "personal",
  "accounts": [
    { "id": "personal", "name": "个人账号", "email": "member@example.com" },
    { "id": "work", "name": "工作账号", "email": "developer@example.com" }
  ]
}
```

邮箱来自各账号工作区页面的官方 `userEmail` hydration 查询。账号列表不会返回 Cookie、工作区 ID 或官网 URL；某个账号读取失败时，其 `email` 为 `null`，不影响其他账号显示。

### 查询用量和记录

```http
GET /api/v1/usage?account=personal&page=0&refresh=false
```

参数：

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `account` | 第一个账号 | `/api/v1/accounts` 返回的账号 ID |
| `page` | `0` | 与官网一致，从 0 开始的记录页码 |
| `refresh` | `false` | `true` 或 `1` 时跳过当前缓存并刷新摘要和本页记录 |

响应示例：

```json
{
  "generated_at": "2026-08-02T07:00:00Z",
  "account": {
    "id": "personal",
    "name": "个人账号",
    "email": "member@example.com"
  },
  "workspace_id": "wrk_xxx",
  "summary": {
    "owned_by_current_user": true,
    "use_balance_after_limit": false,
    "provider_regions": ["us", "eu", "sg", "cn"],
    "five_hours": {
      "status": "ok",
      "cycle": "rolling_5_hours",
      "quota_usd": 12,
      "used_percent": 18,
      "remaining_percent": 82,
      "resets_in_seconds": 6500,
      "resets_at": "2026-08-02T08:48:20Z"
    },
    "seven_days": {},
    "one_month": {},
    "fetched_at": "2026-08-02T07:00:00Z"
  },
  "request_history": {
    "page": 0,
    "page_size": 50,
    "returned": 50,
    "has_previous": false,
    "has_next": true,
    "fetched_at": "2026-08-02T07:00:00Z",
    "records": [
      {
        "id": "usg_xxx",
        "workspace_id": "wrk_xxx",
        "created_at": "2026-08-02T06:59:00Z",
        "model": "deepseek-v4-flash",
        "provider": "inf-go.oa-compat",
        "plan": "go",
        "tokens": {
          "input": 1159,
          "cache_read": 182528,
          "cache_write_5m": 0,
          "cache_write_1h": 0,
          "input_total": 183687,
          "output": 238,
          "reasoning": 87
        },
        "cost": {
          "microcents": 73998,
          "usd": "0.00073998"
        },
        "key_id": "key_xxx",
        "session_id": null
      }
    ]
  },
  "cache": {
    "ttl_seconds": 30,
    "summary_hit": false,
    "records_hit": false
  },
  "source": {
    "go_page": "https://opencode.ai/workspace/wrk_xxx/go",
    "usage_page": "https://opencode.ai/workspace/wrk_xxx/usage",
    "documentation": "https://opencode.ai/docs/zh-cn/go/",
    "transport": "服务端渲染 HTML + 官网 Usage 页面 server-function"
  }
}
```

### 健康检查

```http
GET /api/v1/health
```

健康检查只确认 Salvo 服务可用，并返回 `account_count` 和 `panel_auth_required`；它不要求面板鉴权、不消耗 Cookie，也不访问 OpenCode。

## 分页行为

第 0 页直接解析官网 Usage 页的服务端 hydration 数据。第 1 页及之后调用官网页面本身使用的 server-function。

服务不会硬编码会随官网发布变化的 server-function 哈希。它会从当前 Usage 页的 modulepreload 列表倒序定位路由 bundle，再查找 `usage.list` 对应哈希。如果调用失败，服务会重新抓取页面、重新发现哈希并只重试一次。

`has_next` 与官网规则一致：当本页正好返回 50 条时显示下一页。因此最后一个满页之后可能存在一个空页，这是官网当前行为，不在本地修改。

## Token 口径

官网输入列采用以下合计：

```text
input_total = input + cache_read + cache_write_5m + cache_write_1h
```

仪表盘点击输入 Token 后会显示每个组成项。JSON API 始终返回全部明细。

费用字段保留官网原始 `microcents`，同时提供精确字符串 `usd`。换算关系：

```text
1 USD = 100,000,000 microcents
```

## 错误响应

错误统一返回：

```json
{
  "error": {
    "code": "opencode_authentication_required",
    "message": "OpenCode 登录态无效，请更新当前账号的 Cookie。",
    "hint": "重新登录 opencode.ai，并替换当前账号配置中的 Cookie。"
  }
}
```

常见错误代码：

| 错误代码 | 含义 |
| --- | --- |
| `invalid_query` | 页码或刷新参数无效 |
| `invalid_auth_request` | 面板登录请求不是预期 JSON |
| `panel_authentication_required` | 未登录或面板 Key 错误 |
| `account_not_found` | 指定账号 ID 不存在 |
| `opencode_authentication_required` | Cookie 失效或不完整 |
| `go_subscription_missing` | 工作区没有可读取的 Go 订阅 |
| `opencode_network_error` | 无法连接官网 |
| `opencode_format_changed` | 官网 HTML 或 hydration 格式变化 |
| `opencode_pagination_failed` | 官网私有翻页通道变化或临时失败 |

## 安全说明

- 默认只监听本机地址。公网部署应通过 HTTPS 反向代理，并设置非空的高强度 `server.panel_key`。
- `config.example.json` 是容器示例，容器内监听 `0.0.0.0`；本机运行应使用 `config.local.example.json`。
- 反向代理应覆盖而不是透传客户端提供的 `X-Forwarded-Proto`，HTTPS 请求设置为 `https`，以便会话 Cookie 带 `Secure`。
- 官网基础地址若不是回环地址，配置加载时强制要求 HTTPS；URL 中禁止嵌入用户名、密码、查询参数或片段。
- Cookie 通过 reqwest 敏感 HeaderValue 保存，配置类型不实现 `Debug`。
- 不同账号使用不同 reqwest 客户端和缓存，不共享 Cookie HeaderValue。
- `.gitignore` 默认排除包含凭据的 `config.json`。
- 服务日志不记录请求头、Cookie、官网 HTML 或 server-function 响应正文。
- 所有页面和 API 响应带 `Cache-Control: no-store`。
- 账号邮箱会显示在仪表盘并由 JSON API 返回；只有 `server.panel_key` 非空时这些接口才受面板鉴权保护。
- 仪表盘设置 CSP、`frame-ancestors 'none'` 和 `Referrer-Policy: no-referrer`。
- 解析器只把受支持的 Seroval 纯数据子集转换为 JSON，不使用 JavaScript 引擎，不执行官网代码。
- 服务最多同时处理 256 个客户端连接，并将全部账号合计的官网并发请求限制为 8 个；SIGTERM 和 Ctrl-C 会等待现有请求最多 10 秒后退出。

## 开发检查

`.github/workflows/ci.yml` 会在 push、pull request 和手工触发时执行 Rust 质量门禁与依赖漏洞审计，构建 Linux、Windows、macOS 各自的 x86_64 与 ARM64 二进制产物（Linux 为 MUSL），并验证 Docker 镜像构建。推送严格的 `v*.*.*` 标签时，流水线会先发布对应 GHCR 多架构镜像，再创建 GitHub Release 并附加各平台压缩包及 SHA-256 校验文件。`main` 更新也会发布 `latest` 镜像。

```bash
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo audit
```

项目在 crate 根启用了：

```rust
#![deny(missing_docs)]
```

缺少公开文档注释会直接导致编译失败。

如需用手工抓取且不进入仓库的真实 HTML 验证解析器：

```bash
OPENCODE_REAL_FIXTURE_DIR=/path/to/fixtures \
  cargo test parses_captured_official_pages -- --ignored
```

目录中应包含 `go.html` 和 `usage.html`。这些文件可能含账号、工作区、Key 和请求信息，禁止提交。

## 已知限制

- 每个账号都依赖各自的官网登录 Cookie，Cookie 到期后需要人工更新。
- 用量百分比由官网向下取整，无法恢复官网未公开的精确周期消费额。
- “7d”实际对应官网每周周期，不是本地计算的滚动 168 小时。
- “1 个月”对应官网订阅月周期，不保证是自然月。
- 翻页使用官网未公开的网页内部协议；已做动态发现和显式错误，但官网大改时仍需更新解析器。

## 友情链接

- [LINUX DO 社区](https://linux.do/)
