# localgateway-sdk

Thin Rust runtime SDK for providers that register themselves with LocalGateway.
It reads `HKCU\Software\LocalGateway\port` (override the subkey with
`LOCALGATEWAY_CONFIG_REGISTRY`), performs the `POST /gw/session` handshake and
wraps the generated management client with a one-shot 401 retry.

## Repository layout

This repository root is the crate root: `Cargo.toml`, `src/` and `tests/` live
at the top level, with no nested workspace or crate directory. Current version:
`0.3.0`.

## Dependencies

`localgateway-sdk` depends on `localgateway-api` v0.3.0 through GitHub git + tag:

```toml
localgateway-api = { git = "https://github.com/HuanyuMark/localgateway-api", tag = "v0.3.0", default-features = false }
```

`localgateway-provider` builds on this crate.

## Usage

```toml
[dependencies]
localgateway-sdk = { git = "https://github.com/HuanyuMark/localgateway-sdk", tag = "v0.3.0" }
```

`localgateway-sdk` brings `localgateway-api` with it; do not declare both.

```rust
use localgateway_sdk::{GatewayClient, RegisterOptions};

let client = GatewayClient::new();
let registration = client
    .register("my-service", RegisterOptions::new(8080, "1.0.0", "/health"))
    .await?;
```

## CORS

The gateway answers preflight requests and rewrites `Access-Control-*` headers
for the service, so the business router needs no CORS layer:

```rust
use localgateway_sdk::{Cors, RegisterOptions};

// One flag at registration time: allow every origin, method and header.
client
    .register(
        "my-service",
        RegisterOptions::new(8080, "1.0.0", "/health").cors(Cors::allow_all()),
    )
    .await?;

// Or manage it after registration.
client
    .set_cors(
        "my-service",
        Cors {
            allow_origins: vec!["http://localhost:5173".to_owned()],
            allow_credentials: true,
            max_age_seconds: Some(600),
            ..Cors::default()
        },
    )
    .await?;
client.clear_cors("my-service").await?;
```

`Cors::default()` without `allow_all` requires at least one `allow_origins`
entry, and `max_age_seconds` is capped at 604800 (7 days). Invalid values fail
locally with `SdkError::InvalidCors` before any HTTP call.
