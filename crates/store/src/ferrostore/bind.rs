//! Where `bind_tcp` may listen without TLS (spec §6.3).
//!
//! * A loopback address: always.
//! * The unspecified address (`0.0.0.0`, `::`): never. It listens on every
//!   interface, the ones that are not loopback included.
//! * Any other address: only when the server's `allow_plaintext_on` names
//!   an interface and the address is one of that interface's own addresses
//!   (`getifaddrs`). An IPv6 link-local address also needs the interface's
//!   scope id.
//!
//! With TLS every address is allowed. Authentication is required on every
//! TCP bind, an allowed plaintext interface included.

use super::layout::backend;
use fl_core::StoreError;
use std::ffi::CStr;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// One address of one network interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Interface {
    pub(super) name: String,
    pub(super) ip: IpAddr,
    /// The IPv6 scope id (the interface index of a link-local address);
    /// 0 for IPv4.
    pub(super) scope_id: u32,
}

/// `ip` as IPv4 when it is an IPv4-mapped IPv6 address.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// `true` for a loopback address, an IPv4-mapped one included. This is the
/// address itself; no test switch changes it.
pub(super) fn ip_is_loopback(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn is_link_local_v6(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// `Ok` when `addr` may be bound without TLS, given the interface names in
/// `allow_on` and the host's `interfaces`.
pub(super) fn check_plaintext_bind(
    addr: SocketAddr,
    allow_on: &[String],
    interfaces: &[Interface],
) -> Result<(), StoreError> {
    let ip = canonical(addr.ip());
    if ip.is_unspecified() {
        return Err(backend(format!(
            "bind_tcp refused {addr} without TLS: an unspecified address listens on every \
             interface, the ones that are not loopback included; configure TLS \
             (ServerAuth::with_tls_files), or bind a loopback address or one address of an \
             interface named in allow_plaintext_on"
        )));
    }
    if ip_is_loopback(ip) {
        return Ok(());
    }
    let scope = match addr {
        SocketAddr::V6(v6) if is_link_local_v6(v6.ip()) => Some(v6.scope_id()),
        _ => None,
    };
    let on_allowed = interfaces.iter().any(|iface| {
        allow_on.contains(&iface.name)
            && canonical(iface.ip) == ip
            && scope.is_none_or(|scope| scope == iface.scope_id)
    });
    if on_allowed {
        return Ok(());
    }
    let named = if allow_on.is_empty() {
        "no interface is named in allow_plaintext_on".to_string()
    } else {
        format!(
            "it is not an address of {} (allow_plaintext_on)",
            allow_on.join(", ")
        )
    };
    Err(backend(format!(
        "bind_tcp refused {addr} without TLS: it is not a loopback address and {named}; \
         configure TLS (ServerAuth::with_tls_files), or name its interface in \
         ServerAuth::allow_plaintext_on"
    )))
}

/// Every IPv4 and IPv6 address of every interface of this host.
pub(super) fn interfaces() -> io::Result<Vec<Interface>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out-pointer; on success the list it points
    // to is freed below with `freeifaddrs`, once.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list `getifaddrs` returned, which
        // stays valid until `freeifaddrs`.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_name` is a NUL-terminated string of the list.
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `ifa_addr` is non-null and points to a `sockaddr` whose
        // family says which larger struct it is.
        let family = i32::from(unsafe { (*entry.ifa_addr).sa_family });
        if family == libc::AF_INET {
            // SAFETY: an `AF_INET` address is a `sockaddr_in`.
            let sin = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
            out.push(Interface {
                name,
                ip: IpAddr::V4(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes())),
                scope_id: 0,
            });
        } else if family == libc::AF_INET6 {
            // SAFETY: an `AF_INET6` address is a `sockaddr_in6`.
            let sin6 = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in6>() };
            out.push(Interface {
                name,
                ip: IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)),
                scope_id: sin6.sin6_scope_id,
            });
        }
    }
    // SAFETY: `head` came from a successful `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: &str, scope_id: u32) -> Interface {
        Interface {
            name: name.to_string(),
            ip: ip.parse().unwrap(),
            scope_id,
        }
    }

    fn allowed(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_matcher_needs_a_named_interface_that_holds_the_address() {
        let host = vec![
            iface("lo", "127.0.0.1", 0),
            iface("lo", "::1", 0),
            iface("eth0", "192.168.1.5", 0),
            iface("tailscale0", "100.64.0.7", 0),
            iface("tailscale0", "fd7a:115c:a1e0::7", 0),
            iface("tailscale0", "fe80::1", 4),
            iface("eth0", "fe80::1", 2),
        ];
        let at = |text: &str| -> SocketAddr { text.parse().unwrap() };
        let ok = |addr: &str, names: &[&str]| {
            check_plaintext_bind(at(addr), &allowed(names), &host).is_ok()
        };
        // Loopback: always, named or not.
        for addr in [
            "127.0.0.1:1",
            "127.8.8.8:1",
            "[::1]:1",
            "[::ffff:127.0.0.1]:1",
        ] {
            assert!(ok(addr, &[]), "{addr}");
        }
        // Unspecified: never, whatever is named.
        for addr in ["0.0.0.0:1", "[::]:1", "[::ffff:0.0.0.0]:1"] {
            let err = check_plaintext_bind(at(addr), &allowed(&["lo", "eth0"]), &host)
                .unwrap_err()
                .to_string();
            assert!(err.contains("every interface"), "{addr}: {err}");
        }
        // Not named: refused.
        assert!(!ok("100.64.0.7:1", &[]));
        assert!(!ok("192.168.1.5:1", &["tailscale0"]));
        // Named and on that interface.
        assert!(ok("100.64.0.7:1", &["tailscale0"]));
        assert!(ok("[fd7a:115c:a1e0::7]:1", &["tailscale0"]));
        assert!(ok("[::ffff:100.64.0.7]:1", &["tailscale0"]));
        assert!(ok("192.168.1.5:1", &["eth0", "tailscale0"]));
        // Named, but the address is another interface's, or nobody's.
        assert!(!ok("100.64.0.7:1", &["eth0"]));
        assert!(!ok("10.9.9.9:1", &["eth0", "tailscale0"]));
        // `lo` named does not cover a non-loopback address.
        assert!(!ok("192.168.1.5:1", &["lo"]));
        // A link-local address also needs that interface's scope.
        assert!(ok("[fe80::1%4]:1", &["tailscale0"]));
        assert!(!ok("[fe80::1%2]:1", &["tailscale0"]));
        assert!(ok("[fe80::1%2]:1", &["eth0"]));
        let err = check_plaintext_bind(at("100.64.0.7:1"), &allowed(&["eth0"]), &host)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allow_plaintext_on"), "{err}");
        assert!(err.contains("eth0"), "{err}");
    }

    #[test]
    fn the_host_lists_its_loopback_interface() {
        let host = interfaces().unwrap();
        assert!(
            host.iter()
                .any(|iface| iface.ip == IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "{host:?}"
        );
    }
}
