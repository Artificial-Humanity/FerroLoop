//! Bounded Antigravity identity recovery for the existing CLI adapter.
//!
//! Never enumerate trajectories or choose a transcript by recency. A native UUID comes from
//! the caller's durable record or the environment of a descendant of an owned, exact host unit.

use std::fmt;
use std::time::Instant;

use crate::identity::VendorRoute;

/// All identity evidence belongs to one exact hosted session. Live probing is opt-in: fake
/// runtimes and temporary homes must not accidentally acquire access to the resident host.
pub struct AgySessionRequest<'a> {
    pub hosted_id: &'a str,
    pub host_target: &'a str,
    pub known_cascade_id: Option<&'a str>,
    pub cached_route: Option<&'a VendorRoute>,
    pub allow_host_probe: bool,
    /// Explicit endpoint for isolated hosts; `None` discovers the configured native host.
    pub host_endpoint: Option<&'a fl_host_proto::HostEndpoint>,
}

/// An inseparable connection pair. Credentials never appear in diagnostics or Debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct AgyEndpoint {
    pub address: String,
    pub csrf_token: String,
}

impl fmt::Debug for AgyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgyEndpoint")
            .field("address", &self.address)
            .field("csrf_token", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgyResolvedSession {
    pub cascade_id: Option<String>,
    pub endpoint: Option<AgyEndpoint>,
}

/// Deliberately contains no process contents, host errors, credentials, or paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgyResolutionError {
    #[error("native cascade identity is invalid or conflicts with process evidence")]
    Identity,
    #[error("exact host session ownership or generation could not be verified")]
    Host,
    #[error("bound process is unavailable, changed, or has incomplete evidence")]
    Process,
    #[error("language-server connection evidence is ambiguous or incomplete")]
    Connection,
    #[error("Antigravity discovery deadline or scan limit reached")]
    Limit,
}

/// Validate a native UUID, rejecting persona names, paths, host IDs, and prefixed placeholders.
pub fn native_cascade_id(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    (bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then(|| value.to_ascii_lowercase())
}

/// Recover missing information within the caller's absolute operation deadline. The same
/// deadline must also govern any subsequent RPC. A known UUID alone remains useful for title
/// setup even when no process probe is authorized. Cached persona IDs alone are not ownership
/// proof; a cached connection is accepted only when it names the already-known native UUID.
/// Like `BlockingHostClient`, live probing must run on a synchronous/blocking lane, not directly
/// inside an async task; async callers should use `spawn_blocking`.
pub fn resolve_agy_session(
    request: &AgySessionRequest<'_>,
    deadline: Instant,
) -> Result<AgyResolvedSession, AgyResolutionError> {
    check_deadline(deadline)?;
    let cascade_id = request
        .known_cascade_id
        .map(|id| native_cascade_id(id).ok_or(AgyResolutionError::Identity))
        .transpose()?;
    let mut result = AgyResolvedSession {
        cascade_id,
        endpoint: None,
    };
    if let Some(VendorRoute::Agy {
        conversation_id,
        ls_address: Some(address),
        csrf_token: Some(token),
        ..
    }) = request.cached_route
        && native_cascade_id(conversation_id).as_ref() == result.cascade_id.as_ref()
        && result.cascade_id.is_some()
    {
        result.endpoint = Some(endpoint(address, token)?);
    }
    if !request.allow_host_probe || (result.cascade_id.is_some() && result.endpoint.is_some()) {
        return Ok(result);
    }
    #[cfg(target_os = "linux")]
    {
        linux::resolve_host(request, result, deadline)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(result)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), AgyResolutionError> {
    if Instant::now() >= deadline {
        Err(AgyResolutionError::Limit)
    } else {
        Ok(())
    }
}

fn endpoint(address: &str, token: &str) -> Result<AgyEndpoint, AgyResolutionError> {
    // Stricter than legacy discovery's tolerant log-port parser: no trailing text, user info,
    // URL scheme, or remote address may enter recovered evidence.
    let (host, port) = address
        .rsplit_once(':')
        .ok_or(AgyResolutionError::Connection)?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        || port.is_empty()
        || !port.bytes().all(|b| b.is_ascii_digit())
        || port.parse::<u16>().ok().filter(|p| *p != 0).is_none()
        || token.is_empty()
        || token.len() > 4096
        || token.bytes().any(|b| b.is_ascii_control())
    {
        return Err(AgyResolutionError::Connection);
    }
    Ok(AgyEndpoint {
        address: format!(
            "{}:{port}",
            if host == "localhost" {
                "127.0.0.1"
            } else {
                host
            }
        ),
        csrf_token: token.to_string(),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io::Read;
    use std::path::{Path, PathBuf};

    use fl_host_proto::{AsyncHostClient, ChildState, HostEndpoint, StreamId};

    const MAX_FILE_BYTES: usize = 64 * 1024;
    const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
    const MAX_PROCESSES: usize = 128;
    const MAX_ENTRIES: usize = 1024;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct HostProof {
        stream: StreamId,
        pid: u32,
    }

    async fn host_proof(
        client: &mut AsyncHostClient,
        request: &AgySessionRequest<'_>,
    ) -> Result<HostProof, AgyResolutionError> {
        let wanted = super::super::unit_to_host_session_id(request.host_target)
            .ok_or(AgyResolutionError::Host)?;
        let Some(persona) = request.hosted_id.strip_prefix("antigravity:") else {
            return Err(AgyResolutionError::Host);
        };
        if !wanted
            .as_str()
            .eq_ignore_ascii_case(&format!("{persona}.antigravity"))
        {
            return Err(AgyResolutionError::Host);
        }
        let mut matching = client
            .list_sessions()
            .await
            .map_err(|_| AgyResolutionError::Host)?
            .into_iter()
            .filter(|s| s.stream.session == wanted && matches!(s.state, ChildState::Running(_)));
        let session = matching.next().ok_or(AgyResolutionError::Host)?;
        if matching.next().is_some()
            || !client
                .check_session_owner(session.stream.clone(), request.hosted_id)
                .await
                .map_err(|_| AgyResolutionError::Host)?
        {
            return Err(AgyResolutionError::Host);
        }
        let ChildState::Running(child) = session.state else {
            return Err(AgyResolutionError::Host);
        };
        Ok(HostProof {
            stream: session.stream,
            pid: child.pid,
        })
    }

    pub(super) fn resolve_host(
        request: &AgySessionRequest<'_>,
        initial: AgyResolvedSession,
        deadline: Instant,
    ) -> Result<AgyResolvedSession, AgyResolutionError> {
        // Like BlockingHostClient, this synchronous entry point runs on the caller's
        // blocking lane. spawn_blocking retains a Handle but permits a local block_on.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AgyResolutionError::Host)?;
        runtime.block_on(async {
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                let endpoint = match request.host_endpoint {
                    Some(endpoint) => endpoint.clone(),
                    None => HostEndpoint::discover().map_err(|_| AgyResolutionError::Host)?,
                };
                let mut client = AsyncHostClient::connect(&endpoint)
                    .await
                    .map_err(|_| AgyResolutionError::Host)?;
                let proof = host_proof(&mut client, request).await?;
                let mut probe = ProcProbe::new(Path::new("/proc"), deadline);
                let (result, observed) = probe.resolve(request, initial, proof.pid)?;
                if host_proof(&mut client, request).await? != proof {
                    return Err(AgyResolutionError::Host);
                }
                probe.revalidate(&observed)?;
                check_deadline(deadline)?;
                Ok(result)
            })
            .await
            .map_err(|_| AgyResolutionError::Limit)?
        })
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Process {
        pid: u32,
        parent: u32,
        birth: u64,
    }

    struct ProcProbe<'a> {
        root: &'a Path,
        deadline: Instant,
        remaining: usize,
    }

    impl<'a> ProcProbe<'a> {
        fn new(root: &'a Path, deadline: Instant) -> Self {
            Self {
                root,
                deadline,
                remaining: MAX_TOTAL_BYTES,
            }
        }

        fn read(&mut self, path: &Path) -> Result<Vec<u8>, AgyResolutionError> {
            check_deadline(self.deadline)?;
            let mut bytes = Vec::new();
            fs::File::open(path)
                .map_err(|_| AgyResolutionError::Process)?
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| AgyResolutionError::Process)?;
            if bytes.len() > MAX_FILE_BYTES || bytes.len() > self.remaining {
                return Err(AgyResolutionError::Limit);
            }
            self.remaining -= bytes.len();
            check_deadline(self.deadline)?;
            Ok(bytes)
        }

        fn entries(&self, path: &Path) -> Result<Vec<PathBuf>, AgyResolutionError> {
            check_deadline(self.deadline)?;
            let mut entries = Vec::new();
            for entry in fs::read_dir(path).map_err(|_| AgyResolutionError::Process)? {
                check_deadline(self.deadline)?;
                if entries.len() == MAX_ENTRIES {
                    return Err(AgyResolutionError::Limit);
                }
                entries.push(entry.map_err(|_| AgyResolutionError::Process)?.path());
            }
            Ok(entries)
        }

        fn process(&mut self, pid: u32) -> Result<Process, AgyResolutionError> {
            let bytes = self.read(&self.root.join(pid.to_string()).join("stat"))?;
            let text = std::str::from_utf8(&bytes).map_err(|_| AgyResolutionError::Process)?;
            let (first, tail) = text.rsplit_once(')').ok_or(AgyResolutionError::Process)?;
            let listed_pid = first
                .split_once('(')
                .ok_or(AgyResolutionError::Process)?
                .0
                .trim();
            let fields: Vec<_> = tail.split_whitespace().collect();
            if listed_pid.parse::<u32>().ok() != Some(pid)
                || fields.len() < 20
                || matches!(fields[0], "Z" | "X" | "x")
            {
                return Err(AgyResolutionError::Process);
            }
            Ok(Process {
                pid,
                parent: fields[1].parse().map_err(|_| AgyResolutionError::Process)?,
                birth: fields[19]
                    .parse()
                    .map_err(|_| AgyResolutionError::Process)?,
            })
        }

        fn revalidate(&mut self, processes: &[Process]) -> Result<(), AgyResolutionError> {
            for old in processes {
                if self.process(old.pid)? != *old {
                    return Err(AgyResolutionError::Process);
                }
            }
            Ok(())
        }

        fn resolve(
            &mut self,
            request: &AgySessionRequest<'_>,
            mut result: AgyResolvedSession,
            root_pid: u32,
        ) -> Result<(AgyResolvedSession, Vec<Process>), AgyResolutionError> {
            let root = self.process(root_pid)?;
            let mut pending = vec![(root_pid, root.parent)];
            let mut observed = Vec::new();
            let mut seen = BTreeSet::new();
            let mut connections = Vec::new();
            while let Some((pid, parent)) = pending.pop() {
                if !seen.insert(pid) {
                    continue;
                }
                if seen.len() > MAX_PROCESSES {
                    return Err(AgyResolutionError::Limit);
                }
                let process = self.process(pid)?;
                if process.parent != parent || (pid == root_pid && process != root) {
                    return Err(AgyResolutionError::Process);
                }
                let dir = self.root.join(pid.to_string());
                let env = environment(&self.read(&dir.join("environ"))?)?;
                if let Some(owner) = env.get("FL_HOSTED_ID") {
                    if owner != request.hosted_id {
                        if pid == root_pid {
                            return Err(AgyResolutionError::Host);
                        }
                        // An explicitly foreign nested unit and all of its descendants are
                        // outside this session, even if its process is under our root.
                        continue;
                    }
                } else if pid == root_pid {
                    return Err(AgyResolutionError::Host);
                }
                observed.push(process);
                if pid != root_pid {
                    if let Some(cid) = env.get("ANTIGRAVITY_CONVERSATION_ID") {
                        let cid = native_cascade_id(cid).ok_or(AgyResolutionError::Identity)?;
                        if result
                            .cascade_id
                            .as_ref()
                            .is_some_and(|known| *known != cid)
                        {
                            return Err(AgyResolutionError::Identity);
                        }
                        result.cascade_id = Some(cid);
                    }
                    // Partial pairs are unavailable; never complete one from another PID.
                    if let (Some(addr), Some(token)) = (
                        env.get("ANTIGRAVITY_LS_ADDRESS"),
                        env.get("ANTIGRAVITY_CSRF_TOKEN"),
                    ) {
                        connections.push(endpoint(addr, token)?);
                    }
                    if let Some(connection) = self.language_server(&dir)? {
                        connections.push(connection);
                    }
                }
                for task in self.entries(&dir.join("task"))? {
                    for child in self
                        .read(&task.join("children"))?
                        .split(|b| b.is_ascii_whitespace())
                    {
                        if child.is_empty() {
                            continue;
                        }
                        let child = std::str::from_utf8(child)
                            .ok()
                            .and_then(|s| s.parse::<u32>().ok())
                            .filter(|pid| *pid > 0)
                            .ok_or(AgyResolutionError::Process)?;
                        pending.push((child, pid));
                        if pending.len() > MAX_ENTRIES {
                            return Err(AgyResolutionError::Limit);
                        }
                    }
                }
            }
            for found in connections {
                if result.endpoint.as_ref().is_some_and(|old| old != &found) {
                    return Err(AgyResolutionError::Connection);
                }
                result.endpoint = Some(found);
            }
            self.revalidate(&observed)?;
            Ok((result, observed))
        }

        fn language_server(
            &mut self,
            dir: &Path,
        ) -> Result<Option<AgyEndpoint>, AgyResolutionError> {
            let command = self.read(&dir.join("cmdline"))?;
            let args: Vec<_> = command
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .collect();
            let Some(program) = args.first().and_then(|p| std::str::from_utf8(p).ok()) else {
                return Ok(None);
            };
            if Path::new(program).file_name().and_then(|p| p.to_str())
                != Some("language_server_linux_x64")
            {
                return Ok(None);
            }
            let mut token = None;
            for (i, arg) in args.iter().enumerate().skip(1) {
                let arg = std::str::from_utf8(arg).map_err(|_| AgyResolutionError::Connection)?;
                let value = if arg == "--csrf_token" || arg == "-csrf_token" {
                    Some(*args.get(i + 1).ok_or(AgyResolutionError::Connection)?)
                } else {
                    arg.strip_prefix("--csrf_token=")
                        .or_else(|| arg.strip_prefix("-csrf_token="))
                        .map(str::as_bytes)
                };
                if let Some(value) = value {
                    if token.is_some() {
                        return Err(AgyResolutionError::Connection);
                    }
                    token = Some(
                        std::str::from_utf8(value)
                            .map_err(|_| AgyResolutionError::Connection)?
                            .to_string(),
                    );
                }
            }
            let Some(token) = token else {
                return Ok(None);
            };
            let mut inodes = BTreeSet::new();
            for fd in self.entries(&dir.join("fd"))? {
                check_deadline(self.deadline)?;
                let link = fs::read_link(fd).map_err(|_| AgyResolutionError::Process)?;
                if let Some(inode) = link
                    .to_str()
                    .and_then(|s| s.strip_prefix("socket:["))
                    .and_then(|s| s.strip_suffix(']'))
                {
                    inodes.insert(inode.to_string());
                }
            }
            let mut addresses = BTreeSet::new();
            for (file, ipv6) in [("tcp", false), ("tcp6", true)] {
                let bytes = self.read(&dir.join("net").join(file))?;
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| AgyResolutionError::Connection)?;
                for line in text.lines().skip(1) {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    if fields.len() < 10 || fields[3] != "0A" || !inodes.contains(fields[9]) {
                        continue;
                    }
                    let (address, port) = fields[1]
                        .split_once(':')
                        .ok_or(AgyResolutionError::Connection)?;
                    let host = if !ipv6 && address == "0100007F" {
                        "127.0.0.1"
                    } else if ipv6 && address == "00000000000000000000000001000000" {
                        "[::1]"
                    } else {
                        continue;
                    };
                    let port = u16::from_str_radix(port, 16)
                        .map_err(|_| AgyResolutionError::Connection)?;
                    addresses.insert(format!("{host}:{port}"));
                }
            }
            if addresses.len() > 1 {
                return Err(AgyResolutionError::Connection);
            }
            addresses
                .first()
                .map(|address| endpoint(address, &token))
                .transpose()
        }
    }

    fn environment(bytes: &[u8]) -> Result<BTreeMap<String, String>, AgyResolutionError> {
        let mut values = BTreeMap::new();
        for entry in bytes.split(|b| *b == 0) {
            let Some(equal) = entry.iter().position(|b| *b == b'=') else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(&entry[..equal]) else {
                continue;
            };
            if !matches!(
                key,
                "FL_HOSTED_ID"
                    | "ANTIGRAVITY_CONVERSATION_ID"
                    | "ANTIGRAVITY_LS_ADDRESS"
                    | "ANTIGRAVITY_CSRF_TOKEN"
            ) {
                continue;
            }
            let value = std::str::from_utf8(&entry[equal + 1..])
                .map_err(|_| AgyResolutionError::Process)?;
            if values.insert(key.to_string(), value.to_string()).is_some() {
                return Err(AgyResolutionError::Process);
            }
        }
        Ok(values)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::symlink;
        use std::time::Duration;
        const CID: &str = "4df33135-e9a4-4f85-adfd-3b09dc339628";
        const OTHER: &str = "6e786011-f4af-43fc-928b-a8c92a5da9fd";
        const OWNER: &str = "antigravity:ada";

        fn request(known: Option<&str>) -> AgySessionRequest<'_> {
            AgySessionRequest {
                hosted_id: OWNER,
                host_target: "Ada.antigravity",
                known_cascade_id: known,
                cached_route: None,
                allow_host_probe: false,
                host_endpoint: None,
            }
        }
        struct Fixture(tempfile::TempDir);
        impl Fixture {
            fn new() -> Self {
                let f = Self(tempfile::tempdir().unwrap());
                f.node(100, 1, OWNER, &[], &[101]);
                f.node(101, 100, OWNER, &[], &[]);
                f
            }
            fn node(
                &self,
                pid: u32,
                parent: u32,
                owner: &str,
                env: &[(&str, &str)],
                children: &[u32],
            ) {
                let dir = self.dir(pid);
                fs::create_dir_all(dir.join(format!("task/{pid}"))).unwrap();
                fs::create_dir_all(dir.join("fd")).unwrap();
                fs::create_dir_all(dir.join("net")).unwrap();
                self.stat(pid, parent, u64::from(pid) * 10);
                let mut bytes = format!("FL_HOSTED_ID={owner}\0").into_bytes();
                for (k, v) in env {
                    bytes.extend_from_slice(format!("{k}={v}\0").as_bytes());
                }
                fs::write(dir.join("environ"), bytes).unwrap();
                fs::write(dir.join("cmdline"), b"/bin/sleep\0").unwrap();
                fs::write(
                    dir.join(format!("task/{pid}/children")),
                    children
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
                .unwrap();
                fs::write(dir.join("net/tcp"), "header\n").unwrap();
                fs::write(dir.join("net/tcp6"), "header\n").unwrap();
            }
            fn stat(&self, pid: u32, parent: u32, birth: u64) {
                let mut tail = vec!["0".to_string(); 20];
                tail[0] = "S".into();
                tail[1] = parent.to_string();
                tail[19] = birth.to_string();
                // Parenthesized command names can themselves contain spaces and ')'.
                fs::write(
                    self.dir(pid).join("stat"),
                    format!("{pid} (a ) process) {}", tail.join(" ")),
                )
                .unwrap();
            }
            fn dir(&self, pid: u32) -> PathBuf {
                self.0.path().join(pid.to_string())
            }
            fn ls(&self, pid: u32, rows: &str, own_inodes: &[u64]) {
                fs::write(
                    self.dir(pid).join("cmdline"),
                    b"/opt/vendor/language_server_linux_x64\0--csrf_token\0fixture-token\0",
                )
                .unwrap();
                fs::write(self.dir(pid).join("net/tcp"), format!("header\n{rows}")).unwrap();
                for (fd, inode) in own_inodes.iter().enumerate() {
                    symlink(
                        format!("socket:[{inode}]"),
                        self.dir(pid).join(format!("fd/{fd}")),
                    )
                    .unwrap();
                }
            }
            fn resolve(
                &self,
                known: Option<&str>,
            ) -> Result<AgyResolvedSession, AgyResolutionError> {
                let initial = AgyResolvedSession {
                    cascade_id: known.map(str::to_string),
                    endpoint: None,
                };
                ProcProbe::new(self.0.path(), Instant::now() + Duration::from_secs(2))
                    .resolve(&request(known), initial, 100)
                    .map(|(r, _)| r)
            }
        }
        fn row(address: &str, port: u16, inode: u64) -> String {
            format!(
                "0: {address}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 {inode}\n"
            )
        }

        #[test]
        fn missing_route_and_brain_recover_descendant_identity_and_pair() {
            let f = Fixture::new();
            f.node(
                101,
                100,
                OWNER,
                &[
                    ("ANTIGRAVITY_CONVERSATION_ID", CID),
                    ("ANTIGRAVITY_LS_ADDRESS", "localhost:2345"),
                    ("ANTIGRAVITY_CSRF_TOKEN", "fixture-token"),
                ],
                &[],
            );
            let found = f.resolve(None).unwrap();
            assert_eq!(found.cascade_id.as_deref(), Some(CID));
            assert_eq!(
                found.endpoint.unwrap(),
                AgyEndpoint {
                    address: "127.0.0.1:2345".into(),
                    csrf_token: "fixture-token".into()
                }
            );
        }

        #[test]
        fn saved_uuid_idle_ls_uses_only_its_owned_listening_socket() {
            let f = Fixture::new();
            f.ls(
                101,
                &(row("0100007F", 2345, 42) + &row("0100007F", 9999, 43)),
                &[42],
            );
            let found = f.resolve(Some(CID)).unwrap();
            assert_eq!(found.cascade_id.as_deref(), Some(CID));
            assert_eq!(found.endpoint.unwrap().address, "127.0.0.1:2345");
            assert_eq!(
                f.resolve(None).unwrap().cascade_id,
                None,
                "listener alone cannot invent native identity"
            );
        }

        #[test]
        fn namespace_listener_without_fd_is_not_a_connection() {
            let f = Fixture::new();
            f.ls(101, &row("0100007F", 2345, 42), &[]);
            let found = f.resolve(Some(CID)).unwrap();
            assert_eq!(found.cascade_id.as_deref(), Some(CID));
            assert_eq!(found.endpoint, None);
        }

        #[test]
        fn multiple_owned_listeners_are_ambiguous() {
            let f = Fixture::new();
            f.ls(
                101,
                &(row("0100007F", 2345, 42) + &row("0100007F", 9999, 43)),
                &[42, 43],
            );
            assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Connection));
        }

        #[test]
        fn ipv6_listener_is_correlated_and_nonloopback_is_ignored() {
            let f = Fixture::new();
            f.ls(101, &row("00000000", 2345, 42), &[42, 43]);
            fs::write(
                f.dir(101).join("net/tcp6"),
                format!(
                    "header\n{}",
                    row("00000000000000000000000001000000", 2346, 43)
                ),
            )
            .unwrap();
            assert_eq!(
                f.resolve(Some(CID)).unwrap().endpoint.unwrap().address,
                "[::1]:2346"
            );
        }

        #[test]
        fn incomplete_pairs_never_mix_across_processes_and_do_not_hide_identity() {
            let f = Fixture::new();
            f.node(
                101,
                100,
                OWNER,
                &[
                    ("ANTIGRAVITY_CONVERSATION_ID", CID),
                    ("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:2345"),
                ],
                &[102],
            );
            f.node(
                102,
                101,
                OWNER,
                &[("ANTIGRAVITY_CSRF_TOKEN", "fixture-token")],
                &[],
            );
            let found = f.resolve(None).unwrap();
            assert_eq!(found.cascade_id.as_deref(), Some(CID));
            assert_eq!(found.endpoint, None);
        }

        #[test]
        fn conflicting_connections_refuse_even_with_matching_cascade() {
            let f = Fixture::new();
            f.node(
                101,
                100,
                OWNER,
                &[
                    ("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:2345"),
                    ("ANTIGRAVITY_CSRF_TOKEN", "one"),
                ],
                &[102],
            );
            f.node(
                102,
                101,
                OWNER,
                &[
                    ("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:2345"),
                    ("ANTIGRAVITY_CSRF_TOKEN", "two"),
                ],
                &[],
            );
            assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Connection));
        }

        #[test]
        fn conflicting_or_placeholder_cascade_ids_refuse() {
            for child_id in [OTHER, "Ada.antigravity", "antigravity:ada", "../escape"] {
                let f = Fixture::new();
                f.node(
                    101,
                    100,
                    OWNER,
                    &[("ANTIGRAVITY_CONVERSATION_ID", child_id)],
                    &[],
                );
                assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Identity));
            }
            let f = Fixture::new();
            f.node(
                101,
                100,
                OWNER,
                &[("ANTIGRAVITY_CONVERSATION_ID", CID)],
                &[102],
            );
            f.node(
                102,
                101,
                OWNER,
                &[("ANTIGRAVITY_CONVERSATION_ID", OTHER)],
                &[],
            );
            assert_eq!(f.resolve(None), Err(AgyResolutionError::Identity));
        }

        #[test]
        fn foreign_root_refuses_and_foreign_nested_unit_is_outside_tree() {
            let f = Fixture::new();
            f.node(
                101,
                100,
                "antigravity:other",
                &[("ANTIGRAVITY_CONVERSATION_ID", OTHER)],
                &[102],
            );
            // This descendant need not exist: the foreign subtree must not be inspected.
            assert_eq!(f.resolve(Some(CID)).unwrap().endpoint, None);
            f.node(100, 1, "foreign:ada", &[], &[101]);
            assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Host));
        }

        #[test]
        fn process_birth_and_parent_are_rechecked() {
            for (parent, birth) in [(100, 9999), (999, 1010)] {
                let f = Fixture::new();
                let mut probe = ProcProbe::new(f.0.path(), Instant::now() + Duration::from_secs(2));
                let (_, observed) = probe
                    .resolve(
                        &request(Some(CID)),
                        AgyResolvedSession {
                            cascade_id: Some(CID.into()),
                            endpoint: None,
                        },
                        100,
                    )
                    .unwrap();
                f.stat(101, parent, birth);
                assert_eq!(
                    probe.revalidate(&observed),
                    Err(AgyResolutionError::Process)
                );
            }
        }

        #[test]
        fn root_inherited_conversation_id_is_not_native_identity_evidence() {
            let f = Fixture::new();
            f.node(
                100,
                1,
                OWNER,
                &[("ANTIGRAVITY_CONVERSATION_ID", OTHER)],
                &[101],
            );
            assert_eq!(f.resolve(None).unwrap().cascade_id, None);
        }

        #[test]
        fn deadline_and_file_and_total_read_caps_fail_closed() {
            let f = Fixture::new();
            let mut probe = ProcProbe::new(f.0.path(), Instant::now());
            assert_eq!(probe.process(100), Err(AgyResolutionError::Limit));
            let mut probe = ProcProbe::new(f.0.path(), Instant::now() + Duration::from_secs(2));
            probe.remaining = 0;
            assert_eq!(probe.process(100), Err(AgyResolutionError::Limit));
            fs::write(f.dir(101).join("environ"), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
            assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Limit));
        }

        #[test]
        fn process_count_cap_refuses_partial_scan() {
            let f = Fixture::new();
            for pid in 101..=100 + MAX_PROCESSES as u32 {
                let children = if pid == 100 + MAX_PROCESSES as u32 {
                    vec![]
                } else {
                    vec![pid + 1]
                };
                f.node(pid, pid - 1, OWNER, &[], &children);
            }
            assert_eq!(f.resolve(Some(CID)), Err(AgyResolutionError::Limit));
        }

        #[test]
        fn env_duplicate_fields_are_not_accepted() {
            assert_eq!(
                environment(b"ANTIGRAVITY_CSRF_TOKEN=a\0ANTIGRAVITY_CSRF_TOKEN=b\0"),
                Err(AgyResolutionError::Process)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_probe_can_return_only_explicit_native_identity_without_host_access() {
        let cid = "4df33135-e9a4-4f85-adfd-3b09dc339628";
        for known in [None, Some(cid)] {
            let found = resolve_agy_session(
                &AgySessionRequest {
                    hosted_id: "deliberately invalid",
                    host_target: "not a host target",
                    known_cascade_id: known,
                    cached_route: None,
                    allow_host_probe: false,
                    host_endpoint: None,
                },
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(found.cascade_id.as_deref(), known);
            assert_eq!(found.endpoint, None);
        }
    }

    #[test]
    fn cached_route_id_alone_does_not_prove_ownership() {
        let cid = "4df33135-e9a4-4f85-adfd-3b09dc339628";
        let route = VendorRoute::Agy {
            conversation_id: cid.into(),
            ls_pid: None,
            ls_address: Some("localhost:2345".into()),
            csrf_token: Some("secret".into()),
            agentapi_exe: None,
        };
        for known in [None, Some(cid)] {
            let found = resolve_agy_session(
                &AgySessionRequest {
                    hosted_id: "antigravity:ada",
                    host_target: "Ada.antigravity",
                    known_cascade_id: known,
                    cached_route: Some(&route),
                    allow_host_probe: false,
                    host_endpoint: None,
                },
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(found.cascade_id.as_deref(), known);
            assert_eq!(found.endpoint.is_some(), known.is_some());
        }
    }

    #[test]
    fn credentials_are_redacted_and_recovered_endpoint_is_strict() {
        let connection = endpoint("localhost:2345", "never-print-this").unwrap();
        assert!(!format!("{connection:?}").contains("never-print-this"));
        for bad in [
            "https://localhost:2345",
            "evil.test:2345",
            "127.0.0.1:2345suffix",
            "127.0.0.1:0",
        ] {
            assert_eq!(endpoint(bad, "secret"), Err(AgyResolutionError::Connection));
        }
        assert_eq!(
            endpoint("127.0.0.1:2345", "bad\r\ntoken"),
            Err(AgyResolutionError::Connection)
        );
    }
}
