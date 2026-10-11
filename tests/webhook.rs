use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use aw_client_rust::AwClient;
use aw_models::{Bucket, Event};
use aw_watcher_healthconnect::{WATCHER_NAME, server, watcher::Watcher};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request as AxumRequest, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tower::ServiceExt;

#[derive(Default)]
struct Stored {
    buckets: HashMap<String, Bucket>,
    events: HashMap<String, Vec<Event>>,
    // (目标桶, 是否已提交后再报错)，模拟不同的失败时机。
    next_failure: Option<(String, bool)>,
    hang_next_insert: bool,
}

#[derive(Default)]
struct MockState {
    stored: Mutex<Stored>,
    offline: AtomicBool,
    requests: AtomicUsize,
    next_id: AtomicI64,
    python_precision: AtomicBool,
    insert_started: Notify,
    release_insert: Notify,
}

struct MockAw {
    address: SocketAddr,
    state: Arc<MockState>,
    task: JoinHandle<()>,
}

impl MockAw {
    async fn start() -> Self {
        let state = Arc::new(MockState::default());
        let app = Router::new()
            .route("/api/0/info", get(|| async {
                Json(json!({"hostname":"mock","version":"v0.0.0","testing":true,"device_id":"mock"}))
            }))
            .route("/api/0/buckets/{id}", get(get_bucket).post(create_bucket))
            .route("/api/0/buckets/{id}/events", get(get_events).post(insert_events))
            .layer(middleware::from_fn_with_state(state.clone(), availability))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn router(&self, device: &str, token: Option<&str>) -> Router {
        self.router_with_limits(device, token, 16 * 1024 * 1024, Duration::from_secs(5))
    }

    fn router_with_limits(
        &self,
        device: &str,
        token: Option<&str>,
        limit: usize,
        timeout: Duration,
    ) -> Router {
        let client = AwClient::new(
            "127.0.0.1",
            self.address.port(),
            &format!("{WATCHER_NAME}-{device}"),
        )
        .unwrap();
        server::router(
            Arc::new(Watcher::new(client, device.into())),
            token,
            limit,
            timeout,
        )
    }

    async fn event_count(&self) -> usize {
        self.state
            .stored
            .lock()
            .await
            .events
            .values()
            .map(Vec::len)
            .sum()
    }
}

impl Drop for MockAw {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn availability(
    State(state): State<Arc<MockState>>,
    request: AxumRequest,
    next: Next,
) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    if state.offline.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(request).await
}

async fn get_bucket(State(state): State<Arc<MockState>>, Path(id): Path<String>) -> Response {
    match state.stored.lock().await.buckets.get(&id) {
        Some(bucket) => Json(bucket.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn create_bucket(
    State(state): State<Arc<MockState>>,
    Path(id): Path<String>,
    Json(bucket): Json<Bucket>,
) -> StatusCode {
    assert_eq!(id, bucket.id);
    state.stored.lock().await.buckets.insert(id, bucket);
    StatusCode::OK
}

#[derive(Deserialize)]
struct EventQuery {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
}

async fn get_events(
    State(state): State<Arc<MockState>>,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
) -> Json<Vec<Event>> {
    let stored = state.stored.lock().await;
    let mut events = stored.events.get(&id).cloned().unwrap_or_default();
    // 严格开区间，验证 Watcher 的边界外扩确实覆盖零时长事件。
    events.retain(|event| {
        query
            .start
            .is_none_or(|start| event.calculate_endtime() > start)
            && query.end.is_none_or(|end| event.timestamp < end)
    });
    events.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
    events.truncate(query.limit.unwrap_or(1000));
    Json(events)
}

async fn insert_events(
    State(state): State<Arc<MockState>>,
    Path(id): Path<String>,
    Json(mut events): Json<Vec<Event>>,
) -> Response {
    let mut stored = state.stored.lock().await;
    assert!(stored.buckets.contains_key(&id));
    if stored.hang_next_insert {
        stored.hang_next_insert = false;
        drop(stored);
        state.insert_started.notify_one();
        state.release_insert.notified().await;
        stored = state.stored.lock().await;
    }
    let failure = if stored
        .next_failure
        .as_ref()
        .is_some_and(|(target, _)| target == &id)
    {
        stored.next_failure.take()
    } else {
        None
    };
    if failure.as_ref().is_some_and(|(_, commit)| !commit) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    for event in &mut events {
        if state.python_precision.load(Ordering::SeqCst) {
            event.timestamp =
                DateTime::<Utc>::from_timestamp_millis(event.timestamp.timestamp_millis()).unwrap();
            event.duration =
                chrono::Duration::microseconds(event.duration.num_microseconds().unwrap());
        }
        assert!(event.id.is_none(), "Watcher 不得擅自设置服务端事件 ID");
        event.id = Some(state.next_id.fetch_add(1, Ordering::SeqCst));
    }
    stored.events.entry(id).or_default().extend(events.clone());
    if failure.is_some() {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        Json(events).into_response()
    }
}

fn sample() -> Value {
    let mut payload: Value =
        serde_json::from_str(include_str!("../examples/webhook.json")).unwrap();
    payload["test"] = json!(false);
    payload
}

fn request(payload: &Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("Content-Type", "application/json; charset=utf-8");
    if let Some(token) = token {
        builder = builder.header("X-API-Key", token);
    }
    builder.body(Body::from(payload.to_string())).unwrap()
}

async fn send(app: &Router, payload: &Value, token: Option<&str>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request(payload, token)).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn forwards_using_real_aw_client_and_deduplicates_after_restart() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", Some("integration-token"));
    let (status, result) = send(&app, &sample(), Some("integration-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["written"], 6);
    assert_eq!(result["duplicates"], 0);
    assert_eq!(mock.event_count().await, 6);
    {
        let stored = mock.state.stored.lock().await;
        let bucket = &stored.buckets["aw-watcher-healthconnect-activity_phone"];
        assert_eq!(bucket.client, WATCHER_NAME);
        assert_eq!(bucket.hostname, "phone");
        assert_eq!(bucket._type, "healthconnect.activity");
        let exercise = &stored.events["aw-watcher-healthconnect-exercise_phone"][0];
        assert_eq!(exercise.data["type"], "running");
        assert_eq!(exercise.data["data_type"], "exercise");
    }
    drop(app); // 释放 AwClient 单实例锁，模拟进程重启。
    let restarted = mock.router("phone", None);
    let mut retried = sample();
    retried["timestamp"] = json!("2026-05-09T15:00:00Z");
    retried["app_version"] = json!("2.0.0");
    let (status, result) = send(&restarted, &retried, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["written"], 0);
    assert_eq!(result["duplicates"], 6);
    assert_eq!(mock.event_count().await, 6);
}

#[tokio::test]
async fn concurrent_requests_and_duplicates_within_one_batch_are_idempotent() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    let mut payload = sample();
    let step = payload["steps"][0].clone();
    payload["steps"].as_array_mut().unwrap().push(step);
    let (first, second) = tokio::join!(send(&app, &payload, None), send(&app, &payload, None));
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(
        first.1["written"].as_u64().unwrap() + second.1["written"].as_u64().unwrap(),
        6
    );
    assert_eq!(mock.event_count().await, 6);
}

#[tokio::test]
async fn partial_write_and_ambiguous_committed_write_can_be_retried() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    mock.state.stored.lock().await.next_failure =
        Some(("aw-watcher-healthconnect-sleep_phone".into(), true));
    let response = app.clone().oneshot(request(&sample(), None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["Retry-After"], "5");
    // Activity/body/exercise/sleep 已提交，只有最后的 vitals 尚未处理。
    assert_eq!(mock.event_count().await, 5);
    let (status, result) = send(&app, &sample(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["written"], 1);
    assert_eq!(result["duplicates"], 5);
    assert_eq!(mock.event_count().await, 6);
}

#[tokio::test]
async fn unavailable_aw_does_not_acknowledge_data_and_recovers() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    mock.state.offline.store(true, Ordering::SeqCst);
    let (status, body) = send(&app, &sample(), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["success"], false);
    assert_eq!(mock.event_count().await, 0);
    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
    mock.state.offline.store(false, Ordering::SeqCst);
    assert_eq!(send(&app, &sample(), None).await.0, StatusCode::OK);
    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
}

#[tokio::test]
async fn upstream_mock_test_does_not_pollute_health_buckets() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    let mut payload = sample();
    payload["test"] = json!(true);
    let (status, body) = send(&app, &payload, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["test"], true);
    assert_eq!(body["received"], 6);
    assert_eq!(body["written"], 0);
    assert_eq!(mock.event_count().await, 0);
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auth_rejects_before_parsing_or_contacting_aw() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", Some("integration-token"));
    for token in [None, Some("wrong-token")] {
        assert_eq!(
            send(&app, &sample(), token).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let malformed = Request::builder()
        .method("POST")
        .uri("/webhook")
        .body(Body::from("{"))
        .unwrap();
    assert_eq!(
        app.oneshot(malformed).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_batch_is_rejected_before_any_aw_write() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    let mut payload = sample();
    payload["heart_rate"][0]["time"] = json!("invalid-secret-health-value");
    let (status, body) = send(&app, &payload, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("heart_rate[0].time")
    );
    assert!(!body.to_string().contains("invalid-secret-health-value"));
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_oversized_body_bad_json_and_wrong_content_type() {
    let mock = MockAw::start().await;
    let app = mock.router_with_limits("phone", None, 256, Duration::from_secs(5));
    assert_eq!(
        send(&app, &sample(), None).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for (body, content_type, expected) in [
        ("{", "application/json", StatusCode::BAD_REQUEST),
        ("[]", "application/json", StatusCode::UNPROCESSABLE_ENTITY),
        ("{}", "text/plain", StatusCode::UNSUPPORTED_MEDIA_TYPE),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("Content-Type", content_type)
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn timed_out_post_keeps_lock_until_remote_commit_and_retry_is_safe() {
    let mock = MockAw::start().await;
    let app = mock.router_with_limits("phone", None, 16 * 1024 * 1024, Duration::from_millis(100));
    mock.state.stored.lock().await.hang_next_insert = true;
    assert_eq!(
        send(&app, &sample(), None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    mock.state.insert_started.notified().await;
    let requests = mock.state.requests.load(Ordering::SeqCst);
    // 第一条 POST 尚未提交。重试可以超时，但不能越过保留的写锁去读后写。
    assert_eq!(
        send(&app, &sample(), None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), requests);
    assert_eq!(mock.event_count().await, 0);
    mock.state.release_insert.notify_one();
    let (status, result) = send(&app, &sample(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["written"], 0);
    assert_eq!(result["duplicates"], 6);
    assert_eq!(mock.event_count().await, 6);
}

#[tokio::test]
async fn phones_are_isolated_and_existing_incompatible_buckets_are_not_written() {
    let mock = MockAw::start().await;
    let a = mock.router("phone-a", None);
    let b = mock.router("phone-b", None);
    assert_eq!(send(&a, &sample(), None).await.1["written"], 6);
    assert_eq!(send(&b, &sample(), None).await.1["written"], 6);
    assert_eq!(mock.event_count().await, 12);
    mock.state
        .stored
        .lock()
        .await
        .buckets
        .get_mut("aw-watcher-healthconnect-activity_phone-a")
        .unwrap()
        ._type = "other".into();
    assert_eq!(
        send(&a, &sample(), None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(mock.event_count().await, 12);
}

#[tokio::test]
async fn batches_larger_than_default_aw_query_limit_still_deduplicate() {
    let mock = MockAw::start().await;
    let app = mock.router("phone", None);
    let payload = json!({
        "timestamp":"2026-05-09T12:00:00Z", "app_version":"1.2.3",
        "heart_rate": (0..1201).map(|index| {
            let time = DateTime::<Utc>::from_timestamp(1_700_000_000 + index, 0).unwrap();
            json!({"time":time.to_rfc3339(),"bpm":72})
        }).collect::<Vec<_>>()
    });
    assert_eq!(send(&app, &payload, None).await.1["written"], 1201);
    let (_, result) = send(&app, &payload, None).await;
    assert_eq!(result["written"], 0);
    assert_eq!(result["duplicates"], 1201);
    assert_eq!(mock.event_count().await, 1201);
}

#[tokio::test]
async fn millisecond_backend_rounding_does_not_break_nanosecond_record_deduplication() {
    let mock = MockAw::start().await;
    mock.state.python_precision.store(true, Ordering::SeqCst);
    let app = mock.router("phone", None);
    let payload = json!({
        "timestamp":"2026-05-09T12:00:00Z", "app_version":"1.2.3",
        "heart_rate":[{"time":"2026-05-09T08:00:00.123456789Z","bpm":72}],
        "steps":[{"start_time":"2026-05-09T07:00:00.123456789Z",
            "end_time":"2026-05-09T07:00:00.623456999Z","count":10}]
    });
    assert_eq!(send(&app, &payload, None).await.1["written"], 2);
    let (status, result) = send(&app, &payload, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["written"], 0);
    assert_eq!(result["duplicates"], 2);
    assert_eq!(mock.event_count().await, 2);
}

#[tokio::test]
async fn slow_request_body_is_timed_out_before_any_aw_request() {
    let mock = MockAw::start().await;
    let app = mock.router_with_limits("phone", None, 1024, Duration::from_millis(100));
    let body = Body::from_stream(futures_util::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admission_rejects_excess_uploads_before_reading_json() {
    let mock = MockAw::start().await;
    let app = mock.router_with_limits("phone", None, 1024, Duration::from_secs(5));
    let entered = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let entered = entered.clone();
        let started = started.clone();
        let mut counted = false;
        let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
            if !counted {
                counted = true;
                entered.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
            }
            std::task::Poll::Pending::<Option<Result<axum::body::Bytes, std::io::Error>>>
        }));
        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("Content-Type", "application/json")
            .body(body)
            .unwrap();
        let app = app.clone();
        tasks.push(tokio::spawn(
            async move { app.oneshot(request).await.unwrap() },
        ));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while entered.load(Ordering::SeqCst) < 4 {
            started.notified().await;
        }
    })
    .await
    .unwrap();
    let (status, _) = send(&app, &json!({}), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mock.state.requests.load(Ordering::SeqCst), 0);
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    // 放弃请求后许可也应释放；恢复接受模拟健康数据。
    let mut payload = sample();
    payload["test"] = json!(true);
    let large_app = mock.router_with_limits(
        "other-phone",
        None,
        16 * 1024 * 1024,
        Duration::from_secs(5),
    );
    assert_eq!(send(&large_app, &payload, None).await.0, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            &json!({"timestamp":"2026-05-09T12:00:00Z","app_version":"1"}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
}
