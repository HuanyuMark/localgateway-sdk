//! Thin runtime SDK for providers that register themselves with LocalGateway.
//!
//! The crate deliberately stays small: it locates the gateway through
//! `HKCU\Software\LocalGateway\port` (override the subkey with the
//! `LOCALGATEWAY_CONFIG_REGISTRY` environment variable), performs the
//! `POST /gw/session` handshake, and wraps the generated management client with
//! a one-shot 401 retry.
//!
//! The wire types come from the generated `localgateway-api` crate, which is
//! produced from `gateway_admin::openapi::management_openapi()`.

pub mod client;
pub mod error;
pub mod port;

/// Re-export of the generated management client.
pub use localgateway_api as api;

pub use client::{Cors, GatewayClient, GatewayClientBuilder, PatchOptions, RegisterOptions};
pub use error::SdkError;
pub use port::{PortError, PortSource, RegistryPortSource};
