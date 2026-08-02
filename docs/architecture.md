# 架构与维护说明

## 数据流

```text
浏览器 / API 客户端
        |
        +--> /、/api/v1/auth、/api/v1/health（公开且不含账号数据）
        |
        +--> /api/v1/accounts、/api/v1/usage
                   |
                   +--> panel_key 非空：Key Header / HttpOnly 会话验证
                   +--> panel_key 为空：直接放行
                   |
                   +--> AccountRegistry 选择账号
                   |
                   +--> 独立 Cookie / Client / Cache
                              |
                              +--> Go HTML ----------> 5h / weekly / monthly hydration
                              +--> Usage HTML 第 0 页 -> 最近 50 条 hydration
                              +--> Usage 路由 bundle -> 动态发现 server-function ID
                              +--> /_server 第 N 页 -> Seroval 帧 -> 请求记录
```

每个 Cookie 只在对应账号的 Salvo 到 OpenCode 上游请求中使用，不会写入本地 API 响应或仪表盘 JavaScript。

## 模块职责

| 模块 | 职责 |
| --- | --- |
| `config.rs` | 单一 `config.json`、面板 Key、工作区格式、Cookie HeaderValue、超时和缓存配置 |
| `error.rs` | 上游错误分类、HTTP 状态和不泄露敏感数据的公开提示 |
| `model.rs` | 稳定 JSON 模型、Token 口径、套餐名和精确费用换算 |
| `scrape.rs` | hydration 提取、Seroval 子集转 JSON、RPC 帧解码、函数 ID 发现 |
| `opencode.rs` | 账号注册表、每账号 reqwest 连接池、官网访问、翻页重试、页面缓存和 single-flight |
| `web.rs` | 面板会话、账号列表、账号选择、Salvo 路由、查询参数校验、安全响应头和 JSON 封装 |
| `assets/index.html` | 无构建工具的 Key 登录、中文仪表盘、账号切换和 Token 展开交互 |

## 面板鉴权

`server.panel_key` 为空时，鉴权层直接放行。非空时：

1. 浏览器向 `POST /api/v1/auth` 提交 Key。
2. 服务使用固定时间比较验证 Key，不把 Key 写入日志或 URL。
3. 验证成功后设置由 Key 派生的 `HttpOnly`、`SameSite=Strict` 会话 Cookie；勾选“记住 Key”时增加 30 天 `Max-Age`，原始 Key 不进入 Cookie。
4. `/api/v1/accounts` 和 `/api/v1/usage` 同时支持会话 Cookie 或 `X-Panel-Key` 请求头。
5. 修改 Key 并重启后，旧会话令牌无法继续通过验证。

根 HTML、鉴权状态和健康检查不含账号数据，保持公开。`panel_key` 非空时，浏览器只有通过鉴权后才请求并渲染账号邮箱、额度和记录；为空时数据接口按配置要求公开。公网部署必须由 HTTPS 反向代理终止 TLS。

## 为什么不使用无头浏览器

Playwright 或 Chromium 可以直接点击官网下一页，但会引入数百 MB 运行时、独立进程管理、更高内存占用和浏览器版本维护。

官方页面已经把所需数据作为结构化 hydration 和 Seroval 数据发送到浏览器，因此本服务直接解析数据层：

- 首屏不执行 JavaScript。
- 翻页只复现官网当前页面已经发出的 POST 请求。
- HTTP 连接可以复用。
- 容器和本机部署不需要 Chromium。

## hydration 解析

SolidStart 页面会先注册查询结果槽位：

```javascript
_$HY.r["usage.list[...]"] = $R[15] = $R[2]($R[16] = { ... });
```

流式响应随后解析该槽位：

```javascript
$R[22]($R[16], $R[25] = [ ...records ]);
```

解析器先通过查询名找到注册语句，再取最后一个 `$R[n]` 作为结果槽位，最后找到传给该槽位的值表达式。

支持的 Seroval 子集：

- 对象和数组
- 字符串、数字、`null`
- `!0` 和 `!1` 布尔值
- `$R[n] = value` 赋值包装
- `new Date("RFC3339")`
- 纯对象中的裸属性名

不支持且会显式报错的内容：

- 未赋值的 `$R[n]` 共享引用
- 函数执行
- 任意构造器
- DOM 或网络调用
- 无法识别的 JavaScript 表达式

该限制避免把官网返回内容当作可执行代码处理。

## 官网翻页协议

官网当前的记录函数参数序列化格式如下：

```json
{
  "t": {
    "t": 9,
    "i": 0,
    "l": 2,
    "a": [
      { "t": 1, "s": "wrk_xxx" },
      { "t": 0, "s": 1 }
    ],
    "o": 0
  },
  "f": 31,
  "m": []
}
```

请求头包含当前 bundle 中发现的 `X-Server-Id` 和本地递增的 `X-Server-Instance`。

server-function ID 是内容哈希，会随官网发布变化。服务不会在源码中固定该哈希，而是：

1. 读取当前 Usage HTML 的 `modulepreload` 资源。
2. 从列表尾部倒序下载 bundle。
3. 找到 `usage.list` 定义。
4. 取其前方最近的 64 位十六进制哈希。
5. 缓存函数 ID 和对应资源 URL。
6. 调用失败时强制刷新资源并只重试一次。

Seroval 响应帧格式：

```text
;0x00000302;<指定字节数的 JavaScript 数据表达式>
```

服务只读取第一帧根值，并使用与首屏相同的受限数据转换器。

## 多账号隔离

配置加载后，每个账号生成一个独立 `UsageService`：

```text
AccountRegistry
  personal -> UsageService(Client + Cookie A + Cache A)
  work     -> UsageService(Client + Cookie B + Cache B)
```

账号列表接口返回 `id`、`name` 和官网工作区页面解析出的 `email`。用量接口通过 `account` 查询参数选择账号；未传时选择配置中的第一个账号。

Cookie、server-function 缓存、邮箱与摘要缓存和记录页缓存都不跨账号共享。即使两个账号指向同一工作区，也会分别访问和缓存。

## 缓存与并发

每个账号内部对摘要和记录页分别缓存：

```text
go page -> Cached<GoPageData(summary + email)>
page 0  -> Cached<RecordsPage>
page 1  -> Cached<RecordsPage>
...
```

默认 TTL 为 30 秒。最多保留 64 个记录页，插入前清理过期项，达到上限时淘汰最旧页。

每个账号的刷新过程使用一个异步 single-flight 锁。不同账号可以并行刷新；同一账号的自动刷新、手工刷新和 API 调用会被合并，避免冲击官网。

摘要和当前记录页都缺失时，二者通过 `tokio::join!` 并行抓取。

## 周期语义

5 小时窗口使用官网 `analyzeRollingUsage` 结果。

每周窗口使用官网周边界，API 字段为 `seven_days`，同时通过 `cycle: "calendar_week"` 明确它不是滚动 168 小时。

月度窗口使用订阅创建时间对齐，API 通过 `cycle: "subscription_month"` 明确它不是固定自然月。

## 费用语义

OpenCode 数据库使用 microcents：

```text
USD = microcents / 100,000,000
```

服务使用整数拆分生成十进制字符串，不经过 `f64`，避免小额请求的舍入误差。

## 失败策略

| 场景 | 行为 |
| --- | --- |
| 某账号 302 到登录页、401、403 | 仅该账号返回 401，提示更新对应 Cookie |
| 面板未登录或 Key 错误 | 返回 `panel_authentication_required`，不访问 OpenCode |
| 没有 Go hydration 数据 | 返回 404，提示检查订阅和工作区 |
| 网络错误或官网 5xx | 返回 502，不返回旧数据冒充最新值 |
| HTML 查询槽位变化 | 返回 `opencode_format_changed` |
| server-function 调用失败 | 重新发现一次；仍失败则返回 `opencode_pagination_failed` |
| 响应过大或非 UTF-8 | 拒绝处理并返回 502 |

缓存不会作为上游失败时的静默兜底，因为用量数据具有时效性，显式失败比展示未标记的旧额度更安全。

## 维护检查单

官网改版后按以下顺序排查：

1. 确认 Go 页仍包含 `lite.subscription.get`。
2. 确认 Usage 页仍包含 `usage.list`。
3. 对照官方 `lite-section.tsx` 检查窗口字段。
4. 对照官方 `usage-section.tsx` 检查每页数量和记录字段。
5. 在浏览器 Network 中确认 `/_server` 请求参数和帧头。
6. 更新脱敏 fixture 和单元测试。
7. 运行格式化、测试、Clippy 和真实 HTML 忽略测试。
