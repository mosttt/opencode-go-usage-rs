//! OpenCode Go 用量查询服务入口。

#![deny(missing_docs)]

mod config;
mod error;
mod model;
mod opencode;
mod scrape;
mod web;

use std::time::Duration;

use anyhow::Result;
use config::Config;
use opencode::AccountRegistry;
use salvo::prelude::*;
use tokio::signal;
use tracing_subscriber::EnvFilter;

const MAX_CONCURRENT_CONNECTIONS: usize = 256;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_path("config.json")?;
    let bind_addr = config.bind_addr;
    let auth_required = config.panel_key.is_some();
    let weak_panel_key = config.panel_key.as_ref().is_some_and(|key| key.len() < 16);
    let panel_key = config.panel_key;
    let account_count = config.accounts.len();
    let accounts = AccountRegistry::new(config.accounts)?;
    let router = web::router(accounts, panel_key);

    tracing::info!(
        %bind_addr,
        %account_count,
        %auth_required,
        "OpenCode Go usage dashboard started"
    );
    if !bind_addr.ip().is_loopback() {
        tracing::warn!(
            %bind_addr,
            "服务正在监听非回环地址，请通过防火墙或可信反向代理限制访问"
        );
        if !auth_required {
            tracing::warn!("服务监听非回环地址且 server.panel_key 为空，账号数据未受保护");
        }
    }
    if weak_panel_key {
        tracing::warn!("server.panel_key 少于 16 位，公网部署建议改用至少 32 位随机 Key");
    }

    let acceptor = TcpListener::new(bind_addr.to_string()).bind().await;
    let server = Server::new(acceptor).max_connections(MAX_CONCURRENT_CONNECTIONS);
    let handle = server.handle();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("收到退出信号，开始优雅停止服务");
        handle.stop_graceful(Some(GRACEFUL_SHUTDOWN_TIMEOUT));
    });
    server.try_serve(router).await?;
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "无法注册 SIGTERM 处理器，将仅等待 Ctrl-C");
                let _ = signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

fn init_tracing() {
    let filter = EnvFilter::new("opencode_go_usage=info,salvo_core=warn");
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
