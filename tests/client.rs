//! End-to-end tests for the thin runtime against an in-process mock gateway.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use localgateway_sdk::{GatewayClient, PatchOptions, PortError, PortSource, RegisterOptions};

#[derive(Default)]
struct MockState {
    handshakes: AtomicUsize,
    puts: AtomicUsize,
    patches: AtomicUsize,
    deletes: AtomicUsize,
    health_hits: AtomicUsize,
    fail_put_attempts: AtomicUsize,
    reject_all_puts: AtomicBool,
    valid_token: Mutex<String>,
    put_bodies: Mutex<Vec<Value>>,
    patch_bodies: Mutex<Vec<Value>>,
    seen_tokens: Mutex<Vec<String>>,
}

impl MockState {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

fn session_json(token: &str) -> Value {
    json!({
        "token": token,
        "expiresAt": "2026-11-13T20:00:00Z",
        "gateway": {"baseUrl": "http://127.0.0.1:1", "version": "0.1.0", "pid": 10},
        "caller": {"pid": 20, "userSid": "S-1-5-21-1", "imagePath": "C:\\provider.exe"},
        "policy": {
            "probeIntervalMs": 10000,
            "probeTimeoutMs": 2000,
            "offlineAfterMisses": 3,
            "expireAfterMs": 60000
        }
    })
}

fn registration_json(name: &str, revision: i64) -> Value {
    json!({
        "name": name,
        "status": "healthy",
        "lastProbe": "2026-11-13T20:00:00Z",
        "revision": revision
    })
}

fn error_json(code: &str, message: &str) -> Value {
    json!({"code": code, "message": message})
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned)
}

async fn create_session(State(state): State<Arc<MockState>>) -> impl IntoResponse {
    let attempt = state.handshakes.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("token-{attempt}");
    *state.valid_token.lock().expect("token lock") = token.clone();
    axum::Json(session_json(&token))
}

async fn register_service(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let attempt = state.puts.fetch_add(1, Ordering::SeqCst) + 1;
    let token = bearer_token(&headers);
    state
        .seen_tokens
        .lock()
        .expect("token log")
        .push(token.clone().unwrap_or_default());

    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.put_bodies.lock().expect("body log").push(parsed);

    if state.reject_all_puts.load(Ordering::SeqCst)
        || attempt <= state.fail_put_attempts.load(Ordering::SeqCst)
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(error_json("unauthorized", "missing or expired token")),
        )
            .into_response();
    }

    let valid = state.valid_token.lock().expect("token lock").clone();
    if token.as_deref() != Some(valid.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(error_json("unauthorized", "missing or expired token")),
        )
            .into_response();
    }

    axum::Json(registration_json("sdk-test", attempt as i64)).into_response()
}

async fn patch_service(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let attempt = state.patches.fetch_add(1, Ordering::SeqCst) + 1;
    let token = bearer_token(&headers);
    state
        .seen_tokens
        .lock()
        .expect("token log")
        .push(token.clone().unwrap_or_default());
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.patch_bodies.lock().expect("body log").push(parsed);

    let valid = state.valid_token.lock().expect("token lock").clone();
    if token.as_deref() != Some(valid.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(error_json("unauthorized", "missing or expired token")),
        )
            .into_response();
    }

    axum::Json(registration_json("sdk-test", attempt as i64 + 10)).into_response()
}

async fn unregister_service(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    state.deletes.fetch_add(1, Ordering::SeqCst);
    let token = bearer_token(&headers);
    let valid = state.valid_token.lock().expect("token lock").clone();
    if token.as_deref() != Some(valid.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(error_json("unauthorized", "missing or expired token")),
        )
            .into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn health(State(state): State<Arc<MockState>>) -> impl IntoResponse {
    state.health_hits.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({"pid": 10, "status": "ok", "version": "0.1.0"}))
}

fn mock_app(state: Arc<MockState>) -> Router {
    Router::new()
        .route("/gw/session", post(create_session))
        .route(
            "/gw/services/{name}",
            put(register_service)
                .patch(patch_service)
                .delete(unregister_service),
        )
        .route("/gw/health", get(health))
        .with_state(state)
}

async fn spawn_mock(state: Arc<MockState>) -> String {
    let app = mock_app(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock gateway");
    });
    format!("http://{address}")
}

/// Mock gateway that can be stopped so tests can simulate a restart.
struct StoppableMock {
    base_url: String,
    state: Arc<MockState>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl StoppableMock {
    async fn start(state: Arc<MockState>) -> Self {
        let app = mock_app(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stoppable mock gateway");
        let address = listener.local_addr().expect("mock address");
        let (shutdown, mut stop) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = (&mut stop).await;
                })
                .await
                .expect("serve stoppable mock gateway");
        });
        Self {
            base_url: format!("http://{address}"),
            state,
            shutdown: Some(shutdown),
            handle,
        }
    }

    fn port(&self) -> u16 {
        self.base_url
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .expect("mock port")
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.await.expect("mock gateway stops");
    }
}

fn client_for(base_url: &str) -> GatewayClient {
    GatewayClient::builder().base_url(base_url).build()
}

#[tokio::test]
async fn handshake_then_register_sends_expected_fields() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let options = RegisterOptions::new(47821, "1.2.3", "/health")
        .probe_timeout_ms(1500)
        .api_info(json!({"openapi": "3.1.0", "paths": {"/health": {}}}))
        .project_root("D:\\work\\provider")
        .notify_url("http://127.0.0.1:9000/port-changed");

    let response = client
        .register("sdk-test", options)
        .await
        .expect("register succeeds");

    assert_eq!(response.name, "sdk-test");
    assert_eq!(response.revision, 1);
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 1);
    assert_eq!(state.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.seen_tokens.lock().expect("token log").as_slice(),
        ["token-1"]
    );

    let body = state.put_bodies.lock().expect("body log")[0].clone();
    assert_eq!(body["port"], 47821);
    assert_eq!(body["version"], "1.2.3");
    assert_eq!(body["protocol"], "http");
    assert_eq!(body["probe"]["path"], "/health");
    assert_eq!(body["probe"]["timeoutMs"], 1500);
    assert_eq!(body["apiInfo"]["openapi"], "3.1.0");
    assert_eq!(body["projectRoot"], "D:\\work\\provider");
    assert_eq!(body["notifyUrl"], "http://127.0.0.1:9000/port-changed");
}

#[tokio::test]
async fn unauthorized_register_rehandshakes_once_and_retries() {
    let state = MockState::new();
    state.fail_put_attempts.store(1, Ordering::SeqCst);
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let response = client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", "/health"))
        .await
        .expect("second attempt succeeds");

    assert_eq!(response.revision, 2);
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(state.puts.load(Ordering::SeqCst), 2);
    assert_eq!(
        state.seen_tokens.lock().expect("token log").as_slice(),
        ["token-1", "token-2"]
    );
}

#[tokio::test]
async fn unauthorized_retry_is_not_repeated() {
    let state = MockState::new();
    state.reject_all_puts.store(true, Ordering::SeqCst);
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let error = client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", "/health"))
        .await
        .expect_err("still unauthorized");

    assert!(matches!(
        error,
        localgateway_sdk::SdkError::Api { status: 401, .. }
    ));
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(state.puts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn patch_sends_only_the_requested_fields() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let response = client
        .patch(
            "sdk-test",
            PatchOptions::new()
                .probe("/live", Some(500))
                .project_root("D:\\work\\provider"),
        )
        .await
        .expect("patch succeeds");

    assert!(response.revision >= 10);
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 1);
    let body = state.patch_bodies.lock().expect("body log")[0].clone();
    assert_eq!(body["probe"]["path"], "/live");
    assert_eq!(body["probe"]["timeoutMs"], 500);
    assert_eq!(body["projectRoot"], "D:\\work\\provider");
    assert!(body.get("apiInfo").is_none());
}

#[tokio::test]
async fn unregister_uses_the_cached_session_token() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", "/health"))
        .await
        .expect("register succeeds");
    client
        .unregister("sdk-test")
        .await
        .expect("unregister succeeds");

    assert_eq!(state.handshakes.load(Ordering::SeqCst), 1);
    assert_eq!(state.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_probe_is_rejected_before_any_http_call() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let error = client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", ""))
        .await
        .expect_err("missing probe path must fail locally");
    assert!(matches!(
        error,
        localgateway_sdk::SdkError::InvalidRegistration(_)
    ));

    let error = client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", "health"))
        .await
        .expect_err("probe path must start with '/'");
    assert!(matches!(
        error,
        localgateway_sdk::SdkError::InvalidRegistration(_)
    ));

    assert_eq!(state.handshakes.load(Ordering::SeqCst), 0);
    assert_eq!(state.puts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn empty_patch_is_rejected_locally() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let error = client
        .patch("sdk-test", PatchOptions::new())
        .await
        .expect_err("empty patch must fail");
    assert!(matches!(error, localgateway_sdk::SdkError::InvalidPatch(_)));
    assert_eq!(state.patches.load(Ordering::SeqCst), 0);
}

#[derive(Debug)]
struct FakePortSource {
    port: u16,
    calls: Arc<AtomicUsize>,
}

impl PortSource for FakePortSource {
    fn port(&self) -> Result<u16, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.port)
    }
}

#[tokio::test]
async fn port_source_is_used_when_no_base_url_is_injected() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let port = base_url
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .expect("mock port");
    let calls = Arc::new(AtomicUsize::new(0));
    let source = FakePortSource {
        port,
        calls: Arc::clone(&calls),
    };
    let client = GatewayClient::builder()
        .port_source(Arc::new(source))
        .build();

    client
        .register("sdk-test", RegisterOptions::new(47821, "1.2.3", "/health"))
        .await
        .expect("registry-shaped port source is used");

    assert!(calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(state.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn health_does_not_require_a_session() {
    let state = MockState::new();
    let base_url = spawn_mock(Arc::clone(&state)).await;
    let client = client_for(&base_url);

    let health = client.health().await.expect("health succeeds");
    assert_eq!(health.status, "ok");
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 0);
}

#[derive(Debug)]
struct MutablePortSource {
    port: Arc<AtomicU16>,
    calls: Arc<AtomicUsize>,
}

impl PortSource for MutablePortSource {
    fn port(&self) -> Result<u16, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.port.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn set_base_url_redirects_subsequent_calls() {
    let first = StoppableMock::start(MockState::new()).await;
    let second = StoppableMock::start(MockState::new()).await;
    let client = client_for(&first.base_url);

    client.health().await.expect("first gateway responds");
    assert_eq!(first.state.health_hits.load(Ordering::SeqCst), 1);

    client
        .set_base_url(&second.base_url)
        .expect("new base URL is accepted");
    client.health().await.expect("second gateway responds");

    assert_eq!(first.state.health_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second.state.health_hits.load(Ordering::SeqCst), 1);
    assert_eq!(client.base_url().expect("base URL"), second.base_url);
}

#[tokio::test]
async fn refresh_base_url_rereads_the_port_source() {
    let first = StoppableMock::start(MockState::new()).await;
    let second = StoppableMock::start(MockState::new()).await;
    let port = Arc::new(AtomicU16::new(first.port()));
    let calls = Arc::new(AtomicUsize::new(0));
    let client = GatewayClient::builder()
        .port_source(Arc::new(MutablePortSource {
            port: Arc::clone(&port),
            calls: Arc::clone(&calls),
        }))
        .build();

    client.health().await.expect("first gateway responds");
    assert_eq!(first.state.health_hits.load(Ordering::SeqCst), 1);

    port.store(second.port(), Ordering::SeqCst);
    let refreshed = client.refresh_base_url().expect("port source is readable");
    assert_eq!(refreshed, second.base_url);

    client.health().await.expect("second gateway responds");
    assert_eq!(second.state.health_hits.load(Ordering::SeqCst), 1);
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn transport_failure_refreshes_the_port_source_and_retries_once() {
    let first = StoppableMock::start(MockState::new()).await;
    let port = Arc::new(AtomicU16::new(first.port()));
    let calls = Arc::new(AtomicUsize::new(0));
    let client = GatewayClient::builder()
        .port_source(Arc::new(MutablePortSource {
            port: Arc::clone(&port),
            calls: Arc::clone(&calls),
        }))
        .build();

    client.health().await.expect("first gateway responds");
    first.stop().await;

    let second = StoppableMock::start(MockState::new()).await;
    port.store(second.port(), Ordering::SeqCst);

    client
        .health()
        .await
        .expect("connection failure refreshes the base URL and retries");
    assert_eq!(second.state.health_hits.load(Ordering::SeqCst), 1);
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn set_base_url_rejects_empty_urls() {
    let client = client_for("http://127.0.0.1:1");
    let error = client
        .set_base_url("   ")
        .expect_err("empty base URL is rejected");
    assert!(matches!(error, localgateway_sdk::SdkError::Transport(_)));
}
