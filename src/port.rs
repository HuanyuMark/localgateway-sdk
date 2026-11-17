//! Gateway port discovery.
//!
//! The gateway writes the port it is listening on to
//! `HKCU\Software\LocalGateway` as a `REG_DWORD` value named `port`. Older or
//! hand-edited installations may store the same value as a string, so the
//! registry reader accepts both forms.
//!
//! Tests and non-default installations can redirect the lookup with the
//! `LOCALGATEWAY_CONFIG_REGISTRY` environment variable, which accepts the same
//! forms as the gateway: `HKCU:\Software\...`, `HKCU\Software\...`,
//! `HKEY_CURRENT_USER\Software\...` or a bare subkey (`Software\...`).
//! Surrounding whitespace is ignored and blank values fall back to the default
//! subkey.
//!
//! The runtime calls [`PortSource::port`] for every request, so the environment
//! variable is parsed once per process and cached in a [`OnceLock`]; changing it
//! after the first lookup has no effect.

use std::sync::OnceLock;

/// Default registry subkey relative to `HKEY_CURRENT_USER`.
const DEFAULT_REGISTRY_SUBKEY: &str = r"Software\LocalGateway";
/// Environment variable that redirects registry lookups.
const REGISTRY_CONFIG_ENV: &str = "LOCALGATEWAY_CONFIG_REGISTRY";

/// Failure modes for port discovery.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    /// The registry value does not exist under the given subkey.
    #[error("gateway port is not configured under HKCU\\{0}")]
    NotFound(String),
    /// The registry value exists but is not a valid port.
    #[error("gateway port value is invalid: {0}")]
    Invalid(String),
    /// Reading the registry failed.
    #[error("failed to read gateway port from the registry: {0}")]
    Registry(String),
    /// Port discovery is not implemented on this platform.
    #[error("gateway port lookup is not supported on this platform")]
    Unsupported,
}

/// Source of the gateway port.
///
/// Implementations must be cheap and thread-safe; the runtime calls this for
/// each request so that a gateway restart on a new port is picked up.
pub trait PortSource: Send + Sync + 'static {
    /// Return the current gateway port.
    fn port(&self) -> Result<u16, PortError>;
}

/// Reads `HKCU\<configured subkey>\port`.
///
/// The default subkey is `Software\LocalGateway`. Set
/// `LOCALGATEWAY_CONFIG_REGISTRY` to redirect the lookup (parsed once and
/// cached in a [`OnceLock`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct RegistryPortSource;

impl PortSource for RegistryPortSource {
    fn port(&self) -> Result<u16, PortError> {
        read_port_from_subkey(registry_subkey())
    }
}

/// Read the port from an explicit subkey relative to `HKEY_CURRENT_USER`.
///
/// Returns [`PortError::Unsupported`] on non-Windows platforms.
pub(crate) fn read_port_from_subkey(subkey: &str) -> Result<u16, PortError> {
    imp::read_port(subkey)
}

/// Resolve the configured subkey from an environment-variable value.
///
/// Pure so tests can cover normalization without mutating the process
/// environment (which is unsafe in multithreaded tests under edition 2024).
fn resolve_registry_subkey(environment: Option<&str>) -> String {
    environment
        .and_then(normalize_registry_subkey)
        .unwrap_or_else(|| DEFAULT_REGISTRY_SUBKEY.to_owned())
}

/// Cached subkey for the current process.
fn registry_subkey() -> &'static str {
    static SUBKEY: OnceLock<String> = OnceLock::new();
    SUBKEY
        .get_or_init(|| resolve_registry_subkey(std::env::var(REGISTRY_CONFIG_ENV).ok().as_deref()))
}

fn normalize_registry_subkey(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_prefix = strip_root_prefix(trimmed).trim_start_matches('\\');
    (!without_prefix.is_empty()).then(|| without_prefix.to_owned())
}

fn strip_root_prefix(raw: &str) -> &str {
    const PREFIXES: [&str; 3] = [r"HKCU:\", r"HKCU\", r"HKEY_CURRENT_USER\"];
    for prefix in PREFIXES {
        if raw
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return &raw[prefix.len()..];
        }
    }
    raw
}

pub(crate) fn validate_port(port: u32) -> Result<u16, PortError> {
    if (1..=u32::from(u16::MAX)).contains(&port) {
        Ok(port as u16)
    } else {
        Err(PortError::Invalid(format!(
            "must be between 1 and 65535, got {port}"
        )))
    }
}

pub(crate) fn parse_port_text(text: &str) -> Result<u16, PortError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PortError::Invalid("value is empty".to_owned()));
    }
    let parsed: u32 = trimmed
        .parse()
        .map_err(|_| PortError::Invalid(format!("expected an integer, got {text:?}")))?;
    validate_port(parsed)
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE, WIN32_ERROR,
    };
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::PCWSTR;

    use super::{PortError, parse_port_text, validate_port};

    const VALUE: &str = "port";

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    pub(super) fn read_port(subkey: &str) -> Result<u16, PortError> {
        let subkey_wide = wide(subkey);
        let value = wide(VALUE);

        let mut dword = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::addr_of_mut!(dword).cast()),
                Some(&mut size),
            )
        };

        if status == ERROR_SUCCESS {
            return validate_port(dword);
        }
        if status == ERROR_FILE_NOT_FOUND {
            return Err(PortError::NotFound(subkey.to_owned()));
        }
        if status != ERROR_UNSUPPORTED_TYPE {
            return Err(registry_error(status));
        }

        read_string_port(subkey, &subkey_wide, &value)
    }

    fn read_string_port(
        subkey: &str,
        subkey_wide: &[u16],
        value: &[u16],
    ) -> Result<u16, PortError> {
        let mut buffer = vec![0u16; 64];
        loop {
            let mut size = (buffer.len() * std::mem::size_of::<u16>()) as u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey_wide.as_ptr()),
                    PCWSTR(value.as_ptr()),
                    RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                    None,
                    Some(buffer.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };

            if status == ERROR_SUCCESS {
                let capacity = size as usize / std::mem::size_of::<u16>();
                let end = buffer[..capacity.min(buffer.len())]
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(capacity.min(buffer.len()));
                return parse_port_text(&String::from_utf16_lossy(&buffer[..end]));
            }
            if status == ERROR_MORE_DATA && buffer.len() < 4096 {
                let needed = (size as usize / std::mem::size_of::<u16>()).max(buffer.len() * 2);
                buffer.resize(needed, 0);
                continue;
            }
            if status == ERROR_FILE_NOT_FOUND {
                return Err(PortError::NotFound(subkey.to_owned()));
            }
            return Err(registry_error(status));
        }
    }

    fn registry_error(status: WIN32_ERROR) -> PortError {
        PortError::Registry(format!("RegGetValueW failed with error {}", status.0))
    }
}

#[cfg(not(windows))]
mod imp {
    use super::PortError;

    pub(super) fn read_port(_subkey: &str) -> Result<u16, PortError> {
        Err(PortError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_REGISTRY_SUBKEY, PortError, parse_port_text, resolve_registry_subkey, validate_port,
    };

    #[test]
    fn parses_decimal_port_text() {
        assert_eq!(parse_port_text("47821"), Ok(47821));
        assert_eq!(parse_port_text(" 47821\n"), Ok(47821));
        assert_eq!(parse_port_text("1"), Ok(1));
        assert_eq!(parse_port_text("65535"), Ok(65535));
    }

    #[test]
    fn rejects_out_of_range_and_non_numeric_text() {
        assert!(matches!(parse_port_text("0"), Err(PortError::Invalid(_))));
        assert!(matches!(
            parse_port_text("65536"),
            Err(PortError::Invalid(_))
        ));
        assert!(matches!(parse_port_text(""), Err(PortError::Invalid(_))));
        assert!(matches!(
            parse_port_text("47821abc"),
            Err(PortError::Invalid(_))
        ));
    }

    #[test]
    fn validates_dword_range() {
        assert_eq!(validate_port(1), Ok(1));
        assert_eq!(validate_port(65535), Ok(65535));
        assert!(validate_port(0).is_err());
        assert!(validate_port(65536).is_err());
    }

    #[test]
    fn normalizes_registry_root_prefixes() {
        for raw in [
            r"Software\LocalGateway\tests\a",
            r"HKCU:\Software\LocalGateway\tests\a",
            r"hkcu:\Software\LocalGateway\tests\a",
            r"HKCU\Software\LocalGateway\tests\a",
            r"HKEY_CURRENT_USER\Software\LocalGateway\tests\a",
            r"  Software\LocalGateway\tests\a  ",
            r"\Software\LocalGateway\tests\a",
        ] {
            assert_eq!(
                resolve_registry_subkey(Some(raw)),
                r"Software\LocalGateway\tests\a",
                "{raw:?} should normalize to the bare subkey"
            );
        }
    }

    #[test]
    fn falls_back_to_default_subkey_for_blank_values() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("\t\r\n"),
            Some(r"HKCU:\"),
            Some(r"HKCU\"),
            Some(r"HKEY_CURRENT_USER\"),
        ] {
            assert_eq!(
                resolve_registry_subkey(raw),
                DEFAULT_REGISTRY_SUBKEY,
                "{raw:?} should fall back to the default subkey"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn reads_port_from_explicit_sandbox_subkey() {
        use std::os::windows::ffi::OsStrExt;

        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, REG_DWORD, RegCloseKey, RegCreateKeyW, RegDeleteKeyW,
            RegSetValueExW,
        };
        use windows::core::PCWSTR;

        /// Closes and removes the sandbox key even if an assertion panics.
        struct Sandbox {
            path: Vec<u16>,
            key: HKEY,
            open: bool,
        }

        impl Drop for Sandbox {
            fn drop(&mut self) {
                unsafe {
                    if self.open {
                        let _ = RegCloseKey(self.key);
                    }
                    let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(self.path.as_ptr()));
                }
            }
        }

        let subkey = format!(
            r"Software\LocalGateway\tests\rust-sdk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after the Unix epoch")
                .as_nanos()
        );
        let path: Vec<u16> = std::ffi::OsStr::new(&subkey)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut key = HKEY(std::ptr::null_mut());
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), &mut key) };
        assert_eq!(status, ERROR_SUCCESS, "create sandbox key {subkey}");
        let mut sandbox = Sandbox {
            path,
            key,
            open: true,
        };

        let value: Vec<u16> = std::ffi::OsStr::new("port")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let port: u32 = 54_321;
        let bytes = port.to_le_bytes();
        let status = unsafe {
            RegSetValueExW(
                sandbox.key,
                PCWSTR(value.as_ptr()),
                None,
                REG_DWORD,
                Some(bytes.as_slice()),
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "write sandbox port");
        unsafe {
            let _ = RegCloseKey(sandbox.key);
        }
        sandbox.open = false;

        assert_eq!(
            super::read_port_from_subkey(&subkey),
            Ok(u16::try_from(port).expect("test port fits u16"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_value_error_names_the_subkey() {
        let subkey = format!(
            r"Software\LocalGateway\tests\rust-sdk-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after the Unix epoch")
                .as_nanos()
        );
        match super::read_port_from_subkey(&subkey) {
            Err(PortError::NotFound(name)) => assert_eq!(name, subkey),
            other => panic!("expected NotFound for {subkey}, got {other:?}"),
        }
    }
}
