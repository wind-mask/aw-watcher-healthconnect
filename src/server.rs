use std::{sync::Arc, time::Duration};

use tokio::sync::Semaphore;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

use crate::{
    WATCHER_NAME,
    payload::{self, Payload},
    watcher::Watcher,
};

#[derive(Clone)]
struct AppState {
    watcher: Arc<Watcher>,
    token_hash: Option<[u8; 32]>,
    timeout: Duration,
    admission: Arc<Semaphore>,
}

pub fn router(
    watcher: Arc<Watcher>,
    token: Option<&str>,
    max_body_bytes: usize,
    timeout: Duration,
) -> Router {
    let state = AppState {
        watcher,
        token_hash: token.map(|value| Sha256::digest(value.as_bytes()).into()),
        timeout,
        admission: Arc::new(Semaphore::new(4)),
    };
    let webhook = Router::new()
        .route("/webhook", post(receive))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(DefaultBodyLimit::max(max_body_bytes));
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .merge(webhook)
        .with_state(state)
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = state.token_hash {
        let matches = request
            .headers()
            .get("x-api-key")
            .map(|header| <[u8; 32]>::from(Sha256::digest(header.as_bytes())))
            .is_some_and(|actual| bool::from(actual.ct_eq(&expected)));
        if !matches {
            return failure(StatusCode::UNAUTHORIZED, "缺少或错误的 X-API-Key");
        }
    }
    // 认证后、读取请求体前限制同时驻留的批次；超限立即要求上游重试。
    let Ok(_permit) = state.admission.clone().try_acquire_owned() else {
        return retryable_failure("Webhook 接收端繁忙，请稍后重试");
    };
    next.run(request).await
}

async fn index() -> Json<Value> {
    Json(json!({
        "name": WATCHER_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "webhook": "/webhook",
        "format": "JSON",
        "supported_data_types": payload::DATA_TYPES.iter().map(|spec| spec.name).collect::<Vec<_>>()
    }))
}

async fn health(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(
        state.timeout.min(Duration::from_secs(5)),
        state.watcher.check_connection(),
    )
    .await
    {
        Ok(Ok(())) => Json(json!({"status": "ok", "activitywatch": "connected"})).into_response(),
        _ => failure(StatusCode::SERVICE_UNAVAILABLE, "ActivityWatch 不可用"),
    }
}

async fn receive(State(state): State<AppState>, request: Request) -> Response {
    let deadline = tokio::time::Instant::now() + state.timeout;
    let body =
        match tokio::time::timeout_at(deadline, Json::<Payload>::from_request(request, &state))
            .await
        {
            Ok(body) => body,
            Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "读取 Webhook 请求体超时"),
        };
    let payload = match body {
        Ok(Json(payload)) => payload,
        Err(rejection) => {
            // 不回显原始 JSON 或健康数值，避免错误响应泄露敏感信息。
            return failure(
                rejection.status(),
                "请求需为符合 HC Webhook 格式的 application/json",
            );
        }
    };
    let converted = match payload::convert(&payload) {
        Ok(converted) => converted,
        Err(error) => return failure(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
    };
    let count = converted.record_count;
    if tokio::time::Instant::now() >= deadline {
        return retryable_failure("处理 Webhook 批次超时，请重试");
    }
    if payload.test {
        info!(records = count, "手机 Webhook 测试通过；不保存模拟健康数据");
        return Json(json!({"success":true, "test":true, "received":count, "written":0, "duplicates":0, "buckets":{}})).into_response();
    }
    match tokio::time::timeout_at(deadline, state.watcher.clone().write(converted)).await {
        Ok(Ok(result)) => {
            info!(
                device_id = state.watcher.device_id(),
                written = result.written,
                duplicates = result.duplicates,
                "健康数据批次处理完成"
            );
            Json(json!({
                "success":true, "test":false, "received":count,
                "written":result.written, "duplicates":result.duplicates, "buckets":result.buckets
            }))
            .into_response()
        }
        Ok(Err(_)) => {
            warn!("ActivityWatch 写入失败，请检查连接、认证和桶配置；手机可重试");
            retryable_failure("ActivityWatch 写入失败，请重试；已写入的记录会在重试时去重")
        }
        Err(_) => {
            warn!("ActivityWatch 写入超时；手机可重试");
            retryable_failure("ActivityWatch 写入超时，请重试")
        }
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"success":false, "error":message}))).into_response()
}

fn retryable_failure(message: &str) -> Response {
    let mut response = failure(StatusCode::SERVICE_UNAVAILABLE, message);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, "5".parse().unwrap());
    response
}
