# localgateway-sdk

Thin Rust runtime SDK for providers that register themselves with LocalGateway.
It reads `HKCU\Software\LocalGateway\port` (override the subkey with
`LOCALGATEWAY_CONFIG_REGISTRY`), performs the `POST /gw/session` handshake and
wraps the generated management client with a one-shot 401 retry.

## Usage

```toml
[dependencies]
localgateway-sdk = { git = "https://github.com/HuanyuMark/localgateway-sdk", tag = "v0.1.0" }
```

`localgateway-sdk` brings `localgateway-api` with it; do not declare both.

```rust
use localgateway_sdk::{GatewayClient, RegisterOptions};

let client = GatewayClient::new();
let registration = client
    .register("my-service", RegisterOptions::new(8080, "1.0.0", "/health"))
    .await?;
```