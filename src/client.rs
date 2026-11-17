//! Management client with port discovery, session caching and retries.
//!
//! A client built without an explicit base URL resolves the gateway address
//! from its [`PortSource`] and caches it. When a management call fails at the
//! transport layer (the gateway restarted or moved to another port), the
//! client re-reads the port source, refreshes the cached address and retries
//! exactly once. Clients built with an explicit base URL retry once against
//! the same address instead, because a fixed installation does not use the
//! registry.

use std::future::Future;
use std::sync::{Arc, RwLock};

use localgateway_api::apis::configuration::Configuration;
use localgateway_api::apis::{self, gateway_api, services_api};
use localgateway_api::models;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::SdkError;
use crate::port::{PortSource, RegistryPortSource};

/// Probe timeout bounds accepted by `gateway-core`.
const MIN_PROBE_TIMEOUT_MS: u32 = 100;
const MAX_PROBE_TIMEOUT_MS: u32 = 60_000;

/// Options for `PUT /gw/services/{name}`.
#[derive(Debug, Clone)]
pub struct RegisterOptions {
    /// Provider listening port.
    pub port: u16,
    /// Provider version string.
    pub version: String,
    /// Probe path, for example `/health`.
    pub probe_path: String,
    /// Probe timeout in milliseconds; the gateway default applies when absent.
    pub probe_timeout_ms: Option<u32>,
    /// Optional OpenAPI document.
    pub api_info: Option<Value>,
    /// Optional provider project root.
    pub project_root: Option<String>,
    /// Optional port change notification URL.
    pub notify_url: Option<String>,
}

impl RegisterOptions {
    /// Build registration options. Use [`Self::validate`] before sending.
    #[must_use]
    pub fn new(port: u16, version: impl Into<String>, probe_path: impl Into<String>) -> Self {
        Self {
            port,
            version: version.into(),
            probe_path: probe_path.into(),
            probe_timeout_ms: None,
            api_info: None,
            project_root: None,
            notify_url: None,
        }
    }

    /// Set the probe timeout in milliseconds.
    #[must_use]
    pub fn probe_timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.probe_timeout_ms = Some(timeout_ms);
        self
    }

    /// Attach an OpenAPI document.
    #[must_use]
    pub fn api_info(mut self, api_info: Value) -> Self {
        self.api_info = Some(api_info);
        self
    }

    /// Set the provider project root.
    #[must_use]
    pub fn project_root(mut self, project_root: impl Into<String>) -> Self {
        self.project_root = Some(project_root.into());
        self
    }

    /// Set the port change notification URL.
    #[must_use]
    pub fn notify_url(mut self, notify_url: impl Into<String>) -> Self {
        self.notify_url = Some(notify_url.into());
        self
    }

    /// Validate the locally checkable parts of the request.
    pub fn validate(&self) -> Result<(), SdkError> {
        self.to_request().map(|_| ())
    }

    fn to_request(&self) -> Result<models::RegisterRequest, SdkError> {
        if self.port == 0 {
            return Err(SdkError::InvalidRegistration(
                "port must be greater than 0".to_owned(),
            ));
        }
        if self.version.trim().is_empty() {
            return Err(SdkError::InvalidRegistration(
                "version must not be empty".to_owned(),
            ));
        }
        validate_probe(&self.probe_path, self.probe_timeout_ms)
            .map_err(SdkError::InvalidRegistration)?;

        Ok(models::RegisterRequest {
            port: i32::from(self.port),
            protocol: Some(models::Protocol::Http),
            version: self.version.clone(),
            project_root: self.project_root.clone().map(Some),
            api_info: self.api_info.clone().map(Some),
            probe: Some(Some(Box::new(models::ProbeConfig {
                path: self.probe_path.clone(),
                timeout_ms: self.probe_timeout_ms.map(|timeout| timeout as i32),
            }))),
            notify_url: self.notify_url.clone().map(Some),
        })
    }
}

/// Probe update inside [`PatchOptions`].
#[derive(Debug, Clone)]
pub struct ProbeUpdate {
    /// Probe path.
    pub path: String,
    /// Probe timeout in milliseconds.
    pub timeout_ms: Option<u32>,
}

/// Options for `PATCH /gw/services/{name}`.
///
/// Only the modified fields are sent. An empty patch is rejected locally.
#[derive(Debug, Clone, Default)]
pub struct PatchOptions {
    api_info: Option<Value>,
    project_root: Option<String>,
    probe: Option<ProbeUpdate>,
}

impl PatchOptions {
    /// Create an empty patch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the OpenAPI document.
    #[must_use]
    pub fn api_info(mut self, api_info: Value) -> Self {
        self.api_info = Some(api_info);
        self
    }

    /// Replace the project root.
    #[must_use]
    pub fn project_root(mut self, project_root: impl Into<String>) -> Self {
        self.project_root = Some(project_root.into());
        self
    }

    /// Replace the probe configuration.
    #[must_use]
    pub fn probe(mut self, path: impl Into<String>, timeout_ms: Option<u32>) -> Self {
        self.probe = Some(ProbeUpdate {
            path: path.into(),
            timeout_ms,
        });
        self
    }

    /// Whether no field would be changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.api_info.is_none() && self.project_root.is_none() && self.probe.is_none()
    }

    fn to_request(&self) -> Result<models::PatchRequest, SdkError> {
        if self.is_empty() {
            return Err(SdkError::InvalidPatch(
                "patch must change at least one of apiInfo, projectRoot, probe".to_owned(),
            ));
        }
        if let Some(probe) = &self.probe {
            validate_probe(&probe.path, probe.timeout_ms).map_err(SdkError::InvalidPatch)?;
        }

        Ok(models::PatchRequest {
            api_info: self.api_info.clone().map(Some),
            project_root: self.project_root.clone().map(Some),
            probe: self.probe.as_ref().map(|probe| {
                Some(Box::new(models::ProbeConfig {
                    path: probe.path.clone(),
                    timeout_ms: probe.timeout_ms.map(|timeout| timeout as i32),
                }))
            }),
        })
    }
}

fn validate_probe(path: &str, timeout_ms: Option<u32>) -> Result<(), String> {
    if path.is_empty() {
        return Err("probe.path is required".to_owned());
    }
    if !path.starts_with('/') {
        return Err("probe.path must start with '/'".to_owned());
    }
    if path.chars().any(|c| c.is_whitespace() || c == '#') {
        return Err("probe.path must not contain whitespace or '#'".to_owned());
    }
    if let Some(timeout_ms) = timeout_ms
        && !(MIN_PROBE_TIMEOUT_MS..=MAX_PROBE_TIMEOUT_MS).contains(&timeout_ms)
    {
        return Err(format!(
            "probe.timeoutMs must be between {MIN_PROBE_TIMEOUT_MS} and {MAX_PROBE_TIMEOUT_MS}"
        ));
    }
    Ok(())
}

/// Builder for [`GatewayClient`].
pub struct GatewayClientBuilder {
    base_url: Option<String>,
    port_source: Arc<dyn PortSource>,
    http_client: Option<reqwest::Client>,
}

impl Default for GatewayClientBuilder {
    fn default() -> Self {
        Self {
            base_url: None,
            port_source: Arc::new(RegistryPortSource),
            http_client: None,
        }
    }
}

impl GatewayClientBuilder {
    /// Use a fixed gateway base URL instead of reading the registry.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Inject the port source used when no base URL is configured.
    #[must_use]
    pub fn port_source(mut self, port_source: Arc<dyn PortSource>) -> Self {
        self.port_source = port_source;
        self
    }

    /// Inject a preconfigured HTTP client.
    #[must_use]
    pub fn http_client(mut self, http_client: reqwest::Client) -> Self {
        self.http_client = Some(http_client);
        self
    }

    /// Build the client.
    #[must_use]
    pub fn build(self) -> GatewayClient {
        let uses_port_source = self.base_url.is_none();
        GatewayClient {
            base_url: Arc::new(RwLock::new(self.base_url)),
            uses_port_source,
            port_source: self.port_source,
            http_client: self.http_client.unwrap_or_default(),
            session_token: Arc::new(Mutex::new(None)),
        }
    }
}

/// Client for the LocalGateway management API.
///
/// Clones share the cached base URL and session token, which lets a provider
/// hand one clone to its restart-notification endpoint while keeping another
/// for registration and PATCH calls.
#[derive(Clone)]
pub struct GatewayClient {
    base_url: Arc<RwLock<Option<String>>>,
    uses_port_source: bool,
    port_source: Arc<dyn PortSource>,
    http_client: reqwest::Client,
    session_token: Arc<Mutex<Option<String>>>,
}

impl GatewayClient {
    /// Create a client that discovers the gateway port from the registry.
    #[must_use]
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Start configuring a client.
    #[must_use]
    pub fn builder() -> GatewayClientBuilder {
        GatewayClientBuilder::default()
    }

    /// Return the base URL the next management call will use.
    ///
    /// When the client discovers the port, the first call reads it from the
    /// injected [`PortSource`] and caches the result.
    pub fn base_url(&self) -> Result<String, SdkError> {
        self.resolve_base_url()
    }

    /// Point management calls at a new gateway base URL.
    ///
    /// Providers call this from the `gateway_restarting` notification handler
    /// with the `nextBaseUrl` supplied by the gateway. All clones of this
    /// client observe the new address.
    pub fn set_base_url(&self, base_url: impl Into<String>) -> Result<(), SdkError> {
        let base_url = normalize_base_url(&base_url.into())?;
        *self
            .base_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(base_url);
        Ok(())
    }

    /// Re-read the injected [`PortSource`] and cache the resulting base URL.
    ///
    /// This is the fallback used after a transport failure when the gateway
    /// restarted on a different port. It is also public for callers that want
    /// to refresh eagerly after a `gateway_ready` event.
    pub fn refresh_base_url(&self) -> Result<String, SdkError> {
        let base_url = self.discover_base_url()?;
        *self
            .base_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(base_url.clone());
        Ok(base_url)
    }

    /// Force a new `POST /gw/session` handshake and cache its token.
    pub async fn handshake(&self) -> Result<models::SessionResponse, SdkError> {
        let session = self.create_session().await?;
        *self.session_token.lock().await = Some(session.token.clone());
        Ok(session)
    }

    /// Return the cached session token, handshaking when necessary.
    async fn token(&self) -> Result<String, SdkError> {
        let mut token = self.session_token.lock().await;
        if let Some(cached) = token.as_ref() {
            return Ok(cached.clone());
        }
        let session = self.create_session().await?;
        let fresh = session.token.clone();
        *token = Some(fresh.clone());
        Ok(fresh)
    }

    async fn create_session(&self) -> Result<models::SessionResponse, SdkError> {
        let configuration = self.configuration(None)?;
        gateway_api::create_session(&configuration)
            .await
            .map_err(map_api_error)
    }

    /// Forget `token` if it is still the cached one.
    async fn invalidate_token(&self, token: &str) {
        let mut cached = self.session_token.lock().await;
        if cached.as_deref() == Some(token) {
            *cached = None;
        }
    }

    /// Register or update a provider.
    ///
    /// A `401` response silently triggers one new handshake and one retry. A
    /// transport failure refreshes a discovered base URL and retries once.
    pub async fn register(
        &self,
        service_name: &str,
        options: RegisterOptions,
    ) -> Result<models::RegistrationResponse, SdkError> {
        let request = options.to_request()?;
        let service_name = service_name.to_owned();

        self.with_retry(|token| {
            let request = request.clone();
            let service_name = service_name.clone();
            let client = self;
            async move {
                let configuration = client.configuration(Some(&token))?;
                services_api::register_service(&configuration, &service_name, request)
                    .await
                    .map_err(map_api_error)
            }
        })
        .await
    }

    /// Update the apiInfo, project root or probe of a registered provider.
    pub async fn patch(
        &self,
        service_name: &str,
        options: PatchOptions,
    ) -> Result<models::RegistrationResponse, SdkError> {
        let request = options.to_request()?;
        let service_name = service_name.to_owned();

        self.with_retry(|token| {
            let request = request.clone();
            let service_name = service_name.clone();
            let client = self;
            async move {
                let configuration = client.configuration(Some(&token))?;
                services_api::patch_service(&configuration, &service_name, request)
                    .await
                    .map_err(map_api_error)
            }
        })
        .await
    }

    /// Remove a provider registration.
    pub async fn unregister(&self, service_name: &str) -> Result<(), SdkError> {
        let service_name = service_name.to_owned();

        self.with_retry(|token| {
            let service_name = service_name.clone();
            let client = self;
            async move {
                let configuration = client.configuration(Some(&token))?;
                services_api::unregister_service(&configuration, &service_name)
                    .await
                    .map_err(map_api_error)
            }
        })
        .await
    }

    /// Read `GET /gw/health`; this endpoint is anonymous.
    pub async fn health(&self) -> Result<models::HealthResponse, SdkError> {
        self.with_transport_retry(|| async {
            let configuration = self.configuration(None)?;
            gateway_api::get_health(&configuration)
                .await
                .map_err(map_api_error)
        })
        .await
    }

    /// Retry once after a transport failure, refreshing a discovered address.
    async fn with_transport_retry<T, F, Fut>(&self, call: F) -> Result<T, SdkError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, SdkError>>,
    {
        match call().await {
            Err(error) if error.is_transport() => {
                if self.uses_port_source
                    && let Err(_refresh_error) = self.refresh_base_url()
                {
                    return Err(error);
                }
                call().await
            }
            result => result,
        }
    }

    /// Perform one authenticated call, with one 401 retry and one transport
    /// retry (which may itself lead to one 401 retry on the new address).
    async fn with_retry<T, F, Fut>(&self, call: F) -> Result<T, SdkError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, SdkError>>,
    {
        let mut auth_retried = false;
        let mut transport_retried = false;
        loop {
            let (token, result) = match self.token().await {
                Ok(token) => {
                    let result = call(token.clone()).await;
                    (Some(token), result)
                }
                Err(error) => (None, Err(error)),
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error) if error.is_unauthorized() && !auth_retried => {
                    auth_retried = true;
                    if let Some(token) = token {
                        self.invalidate_token(&token).await;
                    }
                }
                Err(error) if error.is_transport() && !transport_retried => {
                    transport_retried = true;
                    if self.uses_port_source
                        && let Err(_refresh_error) = self.refresh_base_url()
                    {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn configuration(&self, token: Option<&str>) -> Result<Configuration, SdkError> {
        Ok(Configuration {
            base_path: self.resolve_base_url()?,
            user_agent: Some(format!("localgateway-sdk/{}", env!("CARGO_PKG_VERSION"))),
            client: self.http_client.clone(),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: token.map(str::to_owned),
            api_key: None,
        })
    }

    fn resolve_base_url(&self) -> Result<String, SdkError> {
        if let Some(base_url) = self.cached_base_url() {
            return normalize_base_url(&base_url);
        }

        let discovered = self.discover_base_url()?;
        let mut cached = self
            .base_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = cached.as_ref() {
            return normalize_base_url(existing);
        }
        *cached = Some(discovered.clone());
        Ok(discovered)
    }

    fn cached_base_url(&self) -> Option<String> {
        self.base_url
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn discover_base_url(&self) -> Result<String, SdkError> {
        let port = self.port_source.port()?;
        Ok(format!("http://127.0.0.1:{port}"))
    }
}

fn normalize_base_url(raw: &str) -> Result<String, SdkError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(SdkError::Transport("gateway base URL is empty".to_owned()));
    }
    Ok(trimmed.to_owned())
}

impl Default for GatewayClient {
    fn default() -> Self {
        Self::new()
    }
}

fn map_api_error<T>(error: apis::Error<T>) -> SdkError {
    match error {
        apis::Error::ResponseError(content) => {
            let payload: Value = serde_json::from_str(&content.content).unwrap_or(Value::Null);
            let code = payload
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if content.content.is_empty() {
                        format!("HTTP {}", content.status)
                    } else {
                        content.content.clone()
                    }
                });
            SdkError::Api {
                status: content.status.as_u16(),
                code,
                message,
            }
        }
        apis::Error::Reqwest(error) => SdkError::Transport(error.to_string()),
        apis::Error::Serde(error) => {
            SdkError::Transport(format!("invalid gateway response: {error}"))
        }
        apis::Error::Io(error) => SdkError::Transport(error.to_string()),
    }
}
