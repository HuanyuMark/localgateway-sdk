# localgateway-sdk

Thin Rust runtime SDK for providers that register themselves with LocalGateway.
It reads `HKCU\Software\LocalGateway\port` (override the subkey with
`LOCALGATEWAY_CONFIG_REGISTRY`), performs the `POST /gw/session` handshake and
wraps the generated management client with a one-shot 401 retry.

## Repository layout

This repository root is the crate root: `Cargo.toml`, `src/` and `tests/` live
at the top level, with no nested workspace or crate directory. Current version:
`0.2.0`.

## Dependencies

`localgateway-sdk` depends on `localgateway-api` v0.2.0 through GitHub git + tag:

```toml
localgateway-api = { git = "https://github.com/HuanyuMark/localgateway-api", tag = "v0.2.0", default-features = false }
```

`localgateway-provider` builds on this crate.

## Usage

```toml
[dependencies]
localgateway-sdk = { git = "https://github.com/HuanyuMark/localgateway-sdk", tag = "v0.2.0" }
```

`localgateway-sdk` brings `localgateway-api` with it; do not declare both.

```rust
use localgateway_sdk::{GatewayClient, RegisterOptions};

let client = GatewayClient::new();
let registration = client
    .register("my-service", RegisterOptions::new(8080, "1.0.0", "/health"))
    .await?;
```
