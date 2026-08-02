# 部署指南

本文覆盖源码二进制、systemd、Docker Compose 和 HTTPS 反向代理部署。

## 部署前检查

1. 服务固定读取进程工作目录中的 `config.json`，不读取命令行参数或环境变量。
2. 公网部署必须设置非空 `server.panel_key`，推荐使用至少 32 位随机 Key：

   ```bash
   openssl rand -hex 32
   ```

3. 将配置权限限制为当前服务用户可读：

   ```bash
   chmod 600 config.json
   ```

4. 不要直接暴露后端端口。使用 HTTPS 反向代理，并让后端只监听回环地址或 Docker 内部网络。

面板 Key 非空即可启用鉴权，最多 256 位且只能包含可见 ASCII 字符。短 Key 可以运行，但服务会记录安全警告。

## 二进制部署

### 从源码构建

项目要求 Rust 1.94 或更高版本：

```bash
rustup toolchain install 1.94.0
rustup override set 1.94.0
cargo build --release --locked
```

生成文件：

```text
target/release/opencode-go-usage
```

GitHub Actions 的 `CI` 工作流也会生成 `opencode-go-usage-linux-x86_64` 构建产物及 SHA-256 校验文件。该产物面向 glibc Linux x86_64；其他平台建议在目标机源码构建或使用 Docker 多架构镜像。

### systemd 安装

以下示例让二进制位于 `/usr/local/bin`，配置位于 `/etc/opencode-go-usage/config.json`：

```bash
sudo useradd --system --home /nonexistent --shell /usr/sbin/nologin opencode-go
sudo install -Dm755 target/release/opencode-go-usage /usr/local/bin/opencode-go-usage
sudo install -d -o root -g opencode-go -m 750 /etc/opencode-go-usage
sudo install -o root -g opencode-go -m 640 config.json /etc/opencode-go-usage/config.json
sudo install -Dm644 deploy/opencode-go-usage.service /etc/systemd/system/opencode-go-usage.service
sudo systemctl daemon-reload
sudo systemctl enable --now opencode-go-usage
```

`deploy/opencode-go-usage.service` 已设置 `WorkingDirectory=/etc/opencode-go-usage`，因此服务会读取正确的配置文件。

检查状态：

```bash
systemctl status opencode-go-usage
journalctl -u opencode-go-usage -f
curl --fail http://127.0.0.1:8787/api/v1/health
```

升级：

```bash
cargo build --release --locked
sudo install -Dm755 target/release/opencode-go-usage /usr/local/bin/opencode-go-usage
sudo systemctl restart opencode-go-usage
```

二进制或 systemd 部署时，推荐保留：

```json
{
  "server": {
    "bind": "127.0.0.1:8787",
    "panel_key": "替换为随机 Key"
  }
}
```

实际 `config.json` 还需要保留完整的缓存、超时、基础 URL 和账号数组。

## Docker 部署

### 准备配置

容器内必须监听所有容器接口，否则端口映射无法访问：

```json
{
  "server": {
    "bind": "0.0.0.0:8787",
    "panel_key": "替换为随机 Key"
  }
}
```

不要把 `config.json` 构建进镜像。仓库中的 `.dockerignore` 已排除该文件，运行时使用只读挂载。

### Docker Compose

```bash
LOCAL_UID="$(id -u)" LOCAL_GID="$(id -g)" docker compose up -d --build
docker compose logs -f dashboard
```

`compose.yaml` 默认只向宿主机 `127.0.0.1:8787` 发布端口，并启用：

- 只读根文件系统
- 只读配置挂载
- 删除全部 Linux capabilities
- `no-new-privileges`
- 与配置文件所有者一致的 UID/GID

更新与停止：

```bash
docker compose build --pull
LOCAL_UID="$(id -u)" LOCAL_GID="$(id -g)" docker compose up -d
docker compose down
```

若端口需要调整：

```bash
PANEL_PORT=9000 LOCAL_UID="$(id -u)" LOCAL_GID="$(id -g)" docker compose up -d
```

### Docker CLI

```bash
docker build -t opencode-go-usage:local .
docker run -d \
  --name opencode-go-usage \
  --restart unless-stopped \
  --user "$(id -u):$(id -g)" \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=16m \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  -p 127.0.0.1:8787:8787 \
  -v "$PWD/config.json:/app/config.json:ro" \
  opencode-go-usage:local
```

发布多架构镜像时可使用 Buildx：

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t registry.example.com/opencode-go-usage:latest \
  --push .
```

## HTTPS 反向代理

### Caddy

```caddyfile
usage.example.com {
    header Strict-Transport-Security "max-age=31536000"
    reverse_proxy 127.0.0.1:8787
}
```

Caddy 会自动申请证书并设置 `X-Forwarded-Proto`。

### Nginx

在 `http` 块中为鉴权接口配置基础限速：

```nginx
limit_req_zone $binary_remote_addr zone=panel_auth:10m rate=10r/m;
```

站点配置：

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name usage.example.com;

    ssl_certificate /etc/letsencrypt/live/usage.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/usage.example.com/privkey.pem;
    add_header Strict-Transport-Security "max-age=31536000" always;

    location = /api/v1/auth {
        limit_req zone=panel_auth burst=5 nodelay;
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Real-IP $remote_addr;
    }

    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

后端依据 `X-Forwarded-Proto: https` 为面板会话添加 `Secure`。反向代理必须覆盖客户端传入的该请求头，且后端端口不得从公网直接访问。

## 公网安全清单

- 使用 HTTPS，并让 HTTP 自动跳转到 HTTPS。
- 设置高强度 `server.panel_key`；短 Key 仅适合临时内网测试。
- 限速 `/api/v1/auth`，并限制高频 `refresh=true` 请求。
- 不在代理访问日志中增加 `Cookie`、`X-Panel-Key` 或请求正文。
- 账号邮箱属于个人信息；共享面板 Key 等同于授权查看全部配置账号的数据。
- “记住 Key”实际保存的是派生会话令牌，有效期 30 天；轮换 Key 可撤销全部旧会话。
- `/api/v1/health` 是公开接口，只返回状态、账号数量和是否启用面板鉴权。
- 定期更新镜像或二进制，并在升级后运行健康检查。

## 故障排查

### 找不到 config.json

服务从当前工作目录读取 `config.json`。systemd 必须设置正确的 `WorkingDirectory`；Docker 必须挂载到 `/app/config.json`。

### Docker 提示 Permission denied

确保 Compose 使用配置文件所有者的 UID/GID：

```bash
LOCAL_UID="$(id -u)" LOCAL_GID="$(id -g)" docker compose up -d
```

### 登录成功后立即回到 Key 页面

确认公网请求使用 HTTPS，代理设置了 `X-Forwarded-Proto: https`，并且浏览器允许该站点的 Cookie。

### 返回 401

- `panel_authentication_required`：面板未登录或 Key 错误。
- `opencode_authentication_required`：某个 OpenCode 账号的登录 Cookie 已失效。
