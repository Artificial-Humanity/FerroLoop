//! Optional process-local guard installed only by isolated test binaries.
use std::io;
use std::path::Path;
use std::sync::OnceLock;

type UnixValidator = fn(&Path) -> io::Result<()>;
type TcpValidator = fn(&str) -> io::Result<()>;
static UNIX_VALIDATOR: OnceLock<UnixValidator> = OnceLock::new();
static TCP_VALIDATOR: OnceLock<TcpValidator> = OnceLock::new();

/// Install before any threads start. The default, including feature-unified production, is off.
#[doc(hidden)]
pub fn install(unix: UnixValidator, tcp: TcpValidator) {
    UNIX_VALIDATOR
        .set(unix)
        .expect("store Unix test validator installed twice");
    TCP_VALIDATOR
        .set(tcp)
        .expect("store TCP test validator installed twice");
}

#[doc(hidden)]
pub fn validate_unix(path: &Path) -> io::Result<()> {
    UNIX_VALIDATOR
        .get()
        .map_or(Ok(()), |validate| validate(path))
}
#[doc(hidden)]
pub(crate) fn active() -> bool {
    UNIX_VALIDATOR.get().is_some() || TCP_VALIDATOR.get().is_some()
}

#[doc(hidden)]
pub fn validate_tcp(target: &str) -> io::Result<()> {
    TCP_VALIDATOR
        .get()
        .map_or(Ok(()), |validate| validate(target))
}

#[cfg(test)]
mod tests {
    #[test]
    fn compiled_feature_is_inactive_until_installed() {
        assert!(
            super::validate_unix(std::path::Path::new(
                "/run/user/1000/ferroloop/fl-store.sock"
            ))
            .is_ok()
        );
        assert!(super::validate_tcp("example.invalid:99").is_ok());
    }
}
