use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use aw_client_rust::AwClient;
use aw_watcher_healthconnect::{WATCHER_NAME, config::Config, server, watcher::Watcher};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    config.validate()?;

    let default_filter = if config.verbose {
        "info,aw_watcher_healthconnect=debug"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    if !config.host.is_loopback() && config.token.is_none() {
        warn!("监听非回环地址但未配置 AW_HEALTHCONNECT_TOKEN；健康数据接收端没有认证");
    }
    let client_name = format!("{WATCHER_NAME}-{}", config.device_id);
    let client = AwClient::new_with_protocol(
        config.aw_protocol.as_str(),
        &config.aw_host,
        config.server_port(),
        &client_name,
        config.aw_api_key.clone(),
    )
    .map_err(|e| anyhow::anyhow!("无法初始化 aw-rust 客户端：{e}"))?;

    let watcher = Arc::new(Watcher::new(client, config.device_id.clone()));
    let app = server::router(
        watcher.clone(),
        config.token.as_deref(),
        config.max_body_bytes,
        config.timeout(),
    );
    let listener = TcpListener::bind(SocketAddr::new(config.host, config.port))
        .await
        .context("无法绑定 Webhook 监听端口")?;

    info!(
        address = %listener.local_addr()?,
        device_id = %config.device_id,
        aw_host = %config.aw_host,
        aw_port = config.server_port(),
        "Watcher 已启动；POST /webhook 接收 JSON，GET /health 检查 ActivityWatch"
    );
    // 不强制启动时连接服务端：AW 可以稍后启动；期间写入请求返回 503 以触发手机重试。
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Webhook HTTP 服务异常退出")?;
    // HTTP 响应超时不取消在途写入，关闭运行时前等待这些后台写入完成。
    watcher.wait_for_idle().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "无法注册 Ctrl-C 信号");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "无法注册 SIGTERM 信号");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("正在关闭 Watcher，等待处理中的请求完成");
}
