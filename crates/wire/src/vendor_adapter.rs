//! Unified 7-vendor CLI adapter/wrapper interface and `fl-host` session mapping.
//!
//! Covers all interactive CLI vendors hosted under `fl-host`:
//! `claude`, `codex`, `antigravity`, `grok`, `muse`, `lemonade`, and `ollama`.

pub use crate::host_runtime::HostRuntimePort;
pub mod antigravity_session {
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
            result.endpoint = Some(AgyEndpoint::new(address, token)?);
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

    impl AgyEndpoint {
        /// Validate a complete loopback address and CSRF token without mixing sources.
        pub fn new(address: &str, token: &str) -> Result<AgyEndpoint, AgyResolutionError> {
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
                .filter(|s| {
                    s.stream.session == wanted && matches!(s.state, ChildState::Running(_))
                });
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
                let mut ambient = None;
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
                    if pid == root_pid {
                        // The launcher can inherit another session's identity and credentials.
                        // Descendants inheriting those same values add no independent evidence.
                        ambient = Some(env);
                    } else {
                        let ambient = ambient.as_ref().ok_or(AgyResolutionError::Process)?;
                        if let Some(cid) = env.get("ANTIGRAVITY_CONVERSATION_ID") {
                            let inherited = ambient.get("ANTIGRAVITY_CONVERSATION_ID");
                            let native = native_cascade_id(cid);
                            let unchanged = inherited.is_some_and(|root_id| {
                                root_id == cid
                                    || native.as_ref().is_some_and(|id| {
                                        native_cascade_id(root_id).as_ref() == Some(id)
                                    })
                            });
                            if !unchanged {
                                let cid = native.ok_or(AgyResolutionError::Identity)?;
                                if result
                                    .cascade_id
                                    .as_ref()
                                    .is_some_and(|known| *known != cid)
                                {
                                    return Err(AgyResolutionError::Identity);
                                }
                                result.cascade_id = Some(cid);
                            }
                        }
                        // Partial pairs are unavailable; never complete one from another PID.
                        if let (Some(addr), Some(token)) = (
                            env.get("ANTIGRAVITY_LS_ADDRESS"),
                            env.get("ANTIGRAVITY_CSRF_TOKEN"),
                        ) {
                            let inherited = ambient
                                .get("ANTIGRAVITY_LS_ADDRESS")
                                .zip(ambient.get("ANTIGRAVITY_CSRF_TOKEN"));
                            let pair = AgyEndpoint::new(addr, token);
                            let unchanged = inherited.is_some_and(|(root_addr, root_token)| {
                                (root_addr == addr && root_token == token)
                                    || pair.as_ref().is_ok_and(|p| {
                                        AgyEndpoint::new(root_addr, root_token).as_ref() == Ok(p)
                                    })
                            });
                            if !unchanged {
                                connections.push(pair?);
                            }
                        }
                        // An owned listener and the LS token arguments are independent evidence,
                        // including when the endpoint also appears in the ambient environment.
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
                    let arg =
                        std::str::from_utf8(arg).map_err(|_| AgyResolutionError::Connection)?;
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
                    .map(|address| AgyEndpoint::new(address, &token))
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
                    let mut probe =
                        ProcProbe::new(f.0.path(), Instant::now() + Duration::from_secs(2));
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
            fn unchanged_root_environment_is_not_descendant_identity_or_connection_proof() {
                let f = Fixture::new();
                let ambient = [
                    ("ANTIGRAVITY_CONVERSATION_ID", OTHER),
                    ("ANTIGRAVITY_LS_ADDRESS", "localhost:2345"),
                    ("ANTIGRAVITY_CSRF_TOKEN", "fixture-token"),
                ];
                f.node(100, 1, OWNER, &ambient, &[101]);
                f.node(101, 100, OWNER, &ambient, &[]);
                assert_eq!(f.resolve(None).unwrap(), AgyResolvedSession::default());
                // A saved identity does not authenticate an inherited endpoint, even when the
                // ambient identity happens to agree with that record.
                for known in [CID, OTHER] {
                    let found = f.resolve(Some(known)).unwrap();
                    assert_eq!(found.cascade_id.as_deref(), Some(known));
                    assert_eq!(found.endpoint, None);
                }
            }

            #[test]
            fn changed_descendant_identity_and_pair_supersede_root_ambient_values() {
                let f = Fixture::new();
                f.node(
                    100,
                    1,
                    OWNER,
                    &[
                        ("ANTIGRAVITY_CONVERSATION_ID", OTHER),
                        ("ANTIGRAVITY_LS_ADDRESS", "localhost:9999"),
                        ("ANTIGRAVITY_CSRF_TOKEN", "ambient-token"),
                    ],
                    &[101],
                );
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
                    found.endpoint,
                    Some(AgyEndpoint::new("localhost:2345", "fixture-token").unwrap())
                );
            }

            #[test]
            fn owned_listener_corroborates_inherited_pair_but_cannot_invent_identity() {
                let f = Fixture::new();
                let ambient = [
                    ("ANTIGRAVITY_CONVERSATION_ID", CID),
                    ("ANTIGRAVITY_LS_ADDRESS", "localhost:2345"),
                    ("ANTIGRAVITY_CSRF_TOKEN", "fixture-token"),
                ];
                f.node(100, 1, OWNER, &ambient, &[101]);
                f.node(101, 100, OWNER, &ambient, &[]);
                f.ls(101, &row("0100007F", 2345, 42), &[42]);
                for known in [None, Some(CID)] {
                    let found = f.resolve(known).unwrap();
                    assert_eq!(found.cascade_id.as_deref(), known);
                    assert_eq!(
                        found.endpoint,
                        Some(AgyEndpoint::new("localhost:2345", "fixture-token").unwrap())
                    );
                }
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
            let connection = AgyEndpoint::new("localhost:2345", "never-print-this").unwrap();
            assert!(!format!("{connection:?}").contains("never-print-this"));
            for bad in [
                "https://localhost:2345",
                "evil.test:2345",
                "127.0.0.1:2345suffix",
                "127.0.0.1:0",
            ] {
                assert_eq!(
                    AgyEndpoint::new(bad, "secret"),
                    Err(AgyResolutionError::Connection)
                );
            }
            assert_eq!(
                AgyEndpoint::new("127.0.0.1:2345", "bad\r\ntoken"),
                Err(AgyResolutionError::Connection)
            );
        }
    }
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use fl_core::permissions::{
    Effect, EvidenceClass, Origin, Posture, ReceiptClass, ReceiptLine, ReceiptSubject, Transport,
    Vendor, VendorFacts, compile_posture,
};
use fl_host_proto::{ChildState, SessionId, SessionSummary};
use serde::{Deserialize, Serialize};

/// The command a runtime session starts in place of a blank shell: run as an argument vector,
/// never through a shell, so no argument needs quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Environment variables set for the command.
    pub env: Vec<(String, String)>,
    /// The program, then its arguments.
    pub argv: Vec<String>,
}

/// Per-session launch options interpreted by the vendor's [`VendorCliAdapter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorLaunchOptions {
    /// Whether the session should start with the vendor's remote-control / remote-interface
    /// bridge enabled when supported by the vendor CLI (`true` by default).
    pub remote_control: bool,
    /// Permission posture compiled into the vendor's launch flags (`Posture::Auto` by default).
    pub posture: Posture,
}

impl Default for VendorLaunchOptions {
    fn default() -> Self {
        Self {
            remote_control: true,
            posture: Posture::Auto,
        }
    }
}

/// One permission receipt line returned by spin-up and remodel (`permission_receipts`).
///
/// Includes `summary` alongside `class` and `evidence` to satisfy the FerroDeck UI contract
/// (`{ class, summary, evidence? }`) as well as the underlying [`ReceiptLine`] fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionReceipt {
    pub subject: ReceiptSubject,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    pub requested: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native: Vec<String>,
    pub class: ReceiptClass,
    pub evidence: EvidenceClass,
    pub note: String,
    pub summary: String,
}

impl From<ReceiptLine> for PermissionReceipt {
    fn from(line: ReceiptLine) -> Self {
        let summary = if line.native.is_empty() {
            format!("{}: {}", line.requested, line.note)
        } else {
            format!(
                "{}: {} ({})",
                line.requested,
                line.native.join(" "),
                line.note
            )
        };
        Self {
            subject: line.subject,
            origin: line.origin,
            effect: line.effect,
            requested: line.requested,
            native: line.native,
            class: line.class,
            evidence: line.evidence,
            note: line.note,
            summary,
        }
    }
}

/// Permission launch arguments and receipts compiled for a vendor and posture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPermissions {
    pub args: Vec<String>,
    pub receipts: Vec<PermissionReceipt>,
}

/// Compile the launch permission arguments and receipts for `vendor` and `posture`.
///
/// The five permission-compiler vendors (`claude`, `codex`, `muse`, `grok`, `antigravity`) run
/// [`compile_posture`]. Vendors without a permission compiler (`lemonade`, `ollama`, and custom
/// prompt-delivery or API vendors) accept only [`Posture::Auto`] with an `unsupported` receipt and
/// refuse any other posture with an actionable error.
pub fn launch_permissions(vendor: &str, posture: Posture) -> Result<LaunchPermissions, String> {
    let v = vendor.trim().to_ascii_lowercase();
    let compiler_vendor = match v.as_str() {
        "claude" => Some(Vendor::Claude),
        "codex" => Some(Vendor::Codex),
        "muse" => Some(Vendor::Muse),
        "grok" => Some(Vendor::Grok),
        "antigravity" => Some(Vendor::Antigravity),
        _ => None,
    };
    if let Some(cv) = compiler_vendor {
        let plan = compile_posture(cv, &VendorFacts::new(Transport::Pty), posture)
            .map_err(|e| e.to_string())?;
        return Ok(LaunchPermissions {
            args: plan.launch_args,
            receipts: plan
                .receipts
                .into_iter()
                .map(PermissionReceipt::from)
                .collect(),
        });
    }
    if posture != Posture::Auto {
        return Err(format!(
            "vendor `{v}` cannot express posture `{posture}`: no permission flags exist for it"
        ));
    }
    Ok(LaunchPermissions {
        args: Vec::new(),
        receipts: vec![PermissionReceipt::from(ReceiptLine {
            subject: ReceiptSubject::Posture,
            origin: Origin::Axis,
            effect: None,
            requested: "posture Auto".to_string(),
            native: Vec::new(),
            class: ReceiptClass::Unsupported,
            evidence: EvidenceClass::Unsupported,
            note: format!(
                "vendor `{v}` has no permission compiler; auto preserves its native launch without permission flags"
            ),
        })],
    })
}

/// Interactive startup modal or trust prompt that the host supervisor dismisses or accepts on
/// first launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPromptRule {
    pub match_text: &'static str,
    pub response_keys: &'static str,
    pub warn_on_miss: bool,
}

/// Unified adapter interface for an interactive vendor CLI hosted in `fl-host`.
pub trait VendorCliAdapter: Send + Sync + std::fmt::Debug {
    /// Canonical lowercase vendor identifier (`"claude"`, `"codex"`, `"antigravity"`, `"grok"`,
    /// `"muse"`, `"lemonade"`, `"ollama"`).
    fn vendor_id(&self) -> &str;

    /// Optional environment variable overriding the binary path (`FL_WIRE_*_BIN`).
    fn binary_env_var(&self) -> Option<&str>;

    /// Default executable name in `$HOME/.local/bin` or `$PATH`.
    fn default_binary(&self) -> &str;

    /// Resolve the executable path using `self.binary_env_var()`, `<home>/.local/bin`, or `$PATH`.
    fn resolve_binary(&self, home: Option<&Path>) -> String {
        let configured = self.binary_env_var().and_then(std::env::var_os);
        resolve_vendor_bin(configured, home, self.default_binary())
    }

    /// Normalize a requested model id for this vendor's CLI flags, or `None` when no `--model`
    /// argument should be passed.
    fn normalize_model(&self, requested: Option<&str>) -> Option<String>;

    /// Whether this vendor CLI supports starting sessions under remote control.
    fn supports_remote_control(&self) -> bool {
        false
    }

    /// CLI arguments that enable remote control for this session when [`VendorLaunchOptions::remote_control`]
    /// is `true`. Handled strictly at the adapter layer.
    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        Vec::new()
    }

    /// Build the full `LaunchSpec` (`env` + `argv`) for spawning a hosted session with default
    /// [`VendorLaunchOptions`] (`remote_control: true`).
    /// `thinking_level` is the vendor-native effort the operator chose; a vendor with no such
    /// launch option ignores it, and a value that is not a plain token never reaches argv.
    fn build_launch_spec(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
    ) -> LaunchSpec {
        self.build_launch_spec_with_options(
            persona,
            model,
            thinking_level,
            prompt,
            home,
            VendorLaunchOptions::default(),
        )
        .expect("default Auto posture is supported by all built-in adapters")
    }

    /// Build the full `LaunchSpec` (`env` + `argv`) with explicit [`VendorLaunchOptions`].
    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String>;

    /// Whether a launch with `thinking_level` (already a plain token) can honour it. The default
    /// refuses: a vendor with no launch option for it must not report a level it never applied.
    /// An `Err` is an actionable message for the operator.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Err(format!(
            "vendor `{}` does not support thinking_level; omit it",
            self.vendor_id()
        ))
    }

    /// Whether a launch on this vendor can express `posture`, returning the compiled CLI flags
    /// and receipts when supported.
    fn check_posture(&self, posture: Posture) -> Result<LaunchPermissions, String> {
        launch_permissions(self.vendor_id(), posture)
    }

    /// Interactive startup prompts to poll and answer right after spawning.
    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &[]
    }

    /// Key sequence used to interrupt an in-flight turn.
    fn interrupt_key(&self) -> &str {
        "C-c"
    }

    /// In-session slash command used to clear/reset context on reforge.
    fn reset_command(&self) -> &str {
        "/clear"
    }

    /// Whether the vendor's TUI accepts bracketed paste (`\x1b[200~...\x1b[201~`).
    fn supports_bracketed_paste(&self) -> bool {
        true
    }
}

/// The Muse model used when none is requested (or the request is unusable).
pub const MUSE_DEFAULT_MODEL: &str = "muse-wonder-full-1.0";

/// The Muse catalog model ids the CLI accepts for `--model`, default first.
pub const MUSE_CATALOG_MODELS: [&str; 3] =
    [MUSE_DEFAULT_MODEL, "muse-wonder-1.0", "muse-spark-1.3"];

/// Unversioned names the CLI rejects ("model ... does not exist"), with the catalog id each means.
const MUSE_MODEL_ALIASES: [(&str, &str); 3] = [
    ("muse-wonder-full", MUSE_DEFAULT_MODEL),
    ("muse-wonder", "muse-wonder-1.0"),
    ("muse-spark", "muse-spark-1.3"),
];

/// Default model used when launching `lemonade chat` without an explicit model.
pub const LEMONADE_DEFAULT_CLI_MODEL: &str = "Gemma-4-26B-A4B-it-GGUF";

/// Default model used when launching `ollama run` without an explicit model.
pub const OLLAMA_DEFAULT_CLI_MODEL: &str = "llama3.2";

/// The text Grok Build 1.0.70 prints when `--yolo` is not enough for a new directory.
pub const GROK_DIRECTORY_TRUST_PROMPT: &str = "Do you trust the contents of this directory";

/// The text Antigravity CLI prints when launching in an untrusted workspace directory.
pub const AGY_PROJECT_TRUST_PROMPT: &str = "Do you trust the contents of this project?";

/// The modal Claude Code prints on first launch when auto mode asks for environment setup.
pub const CLAUDE_AUTO_MODE_PROMPT: &str = "Teach auto mode about your environment";

static CLAUDE_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: CLAUDE_AUTO_MODE_PROMPT,
    response_keys: "Escape",
    warn_on_miss: false,
}];

static GROK_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: GROK_DIRECTORY_TRUST_PROMPT,
    response_keys: "y",
    warn_on_miss: true,
}];

static AGY_STARTUP_RULES: [StartupPromptRule; 1] = [StartupPromptRule {
    match_text: AGY_PROJECT_TRUST_PROMPT,
    response_keys: "Enter",
    warn_on_miss: false,
}];

/// The model id to hand to `muse --model` for a requested `model`.
pub fn normalize_muse_model(requested: Option<&str>) -> String {
    let trimmed = requested.map_or("", str::trim);
    let lower = trimmed.to_ascii_lowercase();
    if let Some(id) = MUSE_CATALOG_MODELS.iter().find(|id| **id == lower) {
        return (*id).to_string();
    }
    if let Some((_, id)) = MUSE_MODEL_ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return (*id).to_string();
    }
    let plain = trimmed.len() <= 64
        && trimmed
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain {
        trimmed.to_string()
    } else {
        MUSE_DEFAULT_MODEL.to_string()
    }
}

/// Split a trailing context-window tag (`[1m]`, `[200k]`: one to four digits, then lower-case
/// `k` or `m`) off a model id. `None` for an id with no such tag.
pub fn split_window_tag(id: &str) -> Option<(&str, &str)> {
    let open = id.rfind('[')?;
    let (base, tag) = id.split_at(open);
    let inner = tag.strip_prefix('[')?.strip_suffix(']')?;
    let bytes = inner.as_bytes();
    let (&unit, digits) = bytes.split_last()?;
    let valid = matches!(unit, b'k' | b'm')
        && (1..=4).contains(&digits.len())
        && digits.iter().all(u8::is_ascii_digit);
    valid.then_some((base, tag))
}

/// `true` for `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`.
pub fn is_plain_model_id(id: &str) -> bool {
    id.len() <= 64
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A plain model id cannot be interpreted as a command-line option. It may end in ONE
/// context-window tag (`opus[1m]`, `sonnet[1m]`: [`split_window_tag`]); no other bracket is
/// accepted.
pub fn normalize_plain_model(requested: Option<&str>) -> Option<String> {
    let id = requested?.trim();
    let base = split_window_tag(id).map_or(id, |(base, _)| base);
    is_plain_model_id(base).then(|| id.to_string())
}

/// Split a trailing `-low`, `-medium` or `-high` off a model id. An id with no such suffix
/// is returned whole with no thinking level.
pub fn split_agy_thinking_suffix(model_id: &str) -> (String, Option<String>) {
    for level in ["low", "medium", "high"] {
        if let Some(base) = model_id.strip_suffix(&format!("-{level}"))
            && !base.is_empty()
        {
            return (base.to_string(), Some(level.to_string()));
        }
    }
    (model_id.to_string(), None)
}

/// The `agy --model` id for `model`.
pub fn agy_cli_model_id(model: &str, thinking_level: Option<&str>) -> String {
    let model = model.trim().to_ascii_lowercase();
    if model.is_empty() {
        return String::new();
    }
    let (base, existing) = split_agy_thinking_suffix(&model);
    let base = match base.as_str() {
        "argon" | "argon-4" | "argon-4.0" | "gemini-4.0-argon" => "gemini-4-argon",
        other => other,
    };
    if base == "gpt-oss-120b" {
        return format!("{base}-medium");
    }
    let explicit = thinking_level
        .map(|level| level.trim().to_ascii_lowercase())
        .filter(|level| matches!(level.as_str(), "low" | "medium" | "high"));
    let effort = explicit
        .or(existing)
        .unwrap_or_else(|| "medium".to_string());
    format!("{base}-{effort}")
}

/// `level` trimmed, when it is a plain token ([`is_plain_model_id`]) that is safe as one argv
/// element or one config value. Empty and non-plain values (quotes, spaces, `;`, newlines) are
/// dropped, never escaped.
pub fn plain_thinking_level(level: Option<&str>) -> Option<&str> {
    level.map(str::trim).filter(|l| is_plain_model_id(l))
}

/// The prompt as a positional CLI argument. A prompt starting with `-` is given a neutral
/// leading word so it cannot be read as a flag.
pub fn muse_prompt_argument(prompt: &str) -> String {
    if prompt.starts_with('-') {
        format!("Prompt: {prompt}")
    } else {
        prompt.to_string()
    }
}

/// `AGENT_PERSONA` (omitted for `codex`, which shares one `codex app-server` daemon across
/// sessions) plus a `PATH` that starts with `<home>/.local/bin` when that directory is not
/// already a `PATH` entry.
pub fn launch_env_for_vendor(
    vendor: &str,
    persona: &str,
    home: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if vendor != "codex" {
        env.push(("AGENT_PERSONA".into(), persona.into()));
    }
    if let Some(path) = path_with_local_bin(home) {
        env.push(("PATH".into(), path));
    }
    env
}

fn path_with_local_bin(home: Option<&Path>) -> Option<String> {
    let local = home?.join(".local").join("bin");
    let current = std::env::var("PATH").unwrap_or_default();
    Some(with_local_bin(&local, &current))
}

/// `path` unchanged when `local` is already a component; otherwise `local` first.
pub fn with_local_bin(local: &Path, path: &str) -> String {
    if std::env::split_paths(path).any(|entry| entry == local) {
        return path.to_string();
    }
    let local = local.to_string_lossy();
    if path.is_empty() {
        local.into_owned()
    } else {
        format!("{local}:{path}")
    }
}

/// Resolve a configured binary, then a user-local binary, then a PATH command.
pub fn resolve_vendor_bin(
    configured: Option<OsString>,
    home: Option<&Path>,
    bin_name: &str,
) -> String {
    if let Some(bin) = configured.filter(|b| !b.is_empty()) {
        return bin.to_string_lossy().into_owned();
    }
    home.map(|h| h.join(".local").join("bin").join(bin_name))
        .filter(|p| p.is_file())
        .map_or_else(
            || bin_name.to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
}

// ---------------------------------------------------------------------------
// 7 Built-in Vendor CLI Adapters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCliAdapter;

impl VendorCliAdapter for ClaudeCliAdapter {
    fn vendor_id(&self) -> &str {
        "claude"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CLAUDE_BIN")
    }

    fn default_binary(&self) -> &str {
        "claude"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested)
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote-control".to_string()]
    }

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        // Place `--remote-control` before `--permission-mode <mode>` so `--remote-control [name]`
        // (which accepts an optional name in Commander.js) is always followed by a `--` flag
        // and never consumes the trailing positional `<prompt>` when `model` and
        // `thinking_level` are both `None`.
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![self.resolve_binary(home)];
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        argv.extend(perms.args);
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        if let Some(level) = plain_thinking_level(thinking_level) {
            argv.extend(["--effort".to_string(), level.to_string()]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &CLAUDE_STARTUP_RULES
    }

    fn interrupt_key(&self) -> &str {
        "Escape"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexCliAdapter;

impl VendorCliAdapter for CodexCliAdapter {
    fn vendor_id(&self) -> &str {
        "codex"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_CODEX_BIN")
    }

    fn default_binary(&self) -> &str {
        "codex"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested).filter(|m| split_window_tag(m).is_none())
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote".to_string(), "unix://".to_string()]
    }

    /// Any plain token is passed through (`--effort` / reasoning effort); it applies with or
    /// without `--model`.
    fn check_thinking_level(&self, _model: Option<&str>, _level: &str) -> Result<(), String> {
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        if let Some(level) = plain_thinking_level(thinking_level) {
            argv.extend([
                "-c".to_string(),
                format!("model_reasoning_effort=\"{level}\""),
            ]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn interrupt_key(&self) -> &str {
        "Escape"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityCliAdapter;

impl VendorCliAdapter for AntigravityCliAdapter {
    fn vendor_id(&self) -> &str {
        "antigravity"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_AGY_BIN")
    }

    fn default_binary(&self) -> &str {
        "agy"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        requested
            .map(|m| agy_cli_model_id(m, None))
            .and_then(|m| normalize_plain_model(Some(&m)))
            .filter(|m| split_window_tag(m).is_none())
    }

    fn supports_remote_control(&self) -> bool {
        true
    }

    fn remote_control_args(&self, _persona: &str) -> Vec<String> {
        vec!["--remote-control".to_string()]
    }

    /// Antigravity takes its level only as the `-low|-medium|-high` suffix of the model id, so
    /// the level needs a model and must be one of those three; `gpt-oss-120b` only runs at medium.
    fn check_thinking_level(&self, model: Option<&str>, level: &str) -> Result<(), String> {
        let level = level.trim().to_ascii_lowercase();
        if !matches!(level.as_str(), "low" | "medium" | "high") {
            return Err(format!(
                "antigravity supports thinking_level low, medium or high, not `{level}`"
            ));
        }
        let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
            return Err(
                "antigravity applies thinking_level only through the model id: pass a model too"
                    .to_string(),
            );
        };
        let (base, _) = split_agy_thinking_suffix(&model.to_ascii_lowercase());
        if base == "gpt-oss-120b" && level != "medium" {
            return Err(format!(
                "model gpt-oss-120b only runs at thinking_level medium, not `{level}`"
            ));
        }
        Ok(())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if options.remote_control {
            argv.extend(self.remote_control_args(persona));
        }
        let cli_model = model
            .map(|m| agy_cli_model_id(m, thinking_level))
            .and_then(|m| normalize_plain_model(Some(&m)))
            .filter(|m| split_window_tag(m).is_none());
        if let Some(m) = cli_model {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push("--prompt-interactive".to_string());
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &AGY_STARTUP_RULES
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GrokCliAdapter;

impl VendorCliAdapter for GrokCliAdapter {
    fn vendor_id(&self) -> &str {
        "grok"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_GROK_BIN")
    }

    fn default_binary(&self) -> &str {
        "grok"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested).filter(|m| split_window_tag(m).is_none())
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![self.resolve_binary(home)];
        argv.extend(perms.args);
        if let Some(m) = self.normalize_model(model) {
            argv.extend(["--model".to_string(), m]);
        }
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn startup_prompt_rules(&self) -> &[StartupPromptRule] {
        &GROK_STARTUP_RULES
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MuseCliAdapter;

impl VendorCliAdapter for MuseCliAdapter {
    fn vendor_id(&self) -> &str {
        "muse"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_MUSE_BIN")
    }

    fn default_binary(&self) -> &str {
        "muse"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        Some(normalize_muse_model(requested))
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![
            self.resolve_binary(home),
            "--model".to_string(),
            normalize_muse_model(model),
        ];
        argv.extend(perms.args);
        argv.push(muse_prompt_argument(prompt));
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }

    fn reset_command(&self) -> &str {
        "/new"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LemonadeCliAdapter;

impl VendorCliAdapter for LemonadeCliAdapter {
    fn vendor_id(&self) -> &str {
        "lemonade"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_LEMONADE_BIN")
    }

    fn default_binary(&self) -> &str {
        "lemonade"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        normalize_plain_model(requested)
            .filter(|m| split_window_tag(m).is_none())
            .or_else(|| Some(LEMONADE_DEFAULT_CLI_MODEL.to_string()))
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let host = std::env::var("FL_LEMONADE_HOST")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("LEMONADE_HOST")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("FL_LEMONADE_PORT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("LEMONADE_PORT")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "13305".to_string());
        let mut argv = vec![
            self.resolve_binary(home),
            "--no-discovery".to_string(),
            "--host".to_string(),
            host,
            "--port".to_string(),
            port,
            "chat".to_string(),
        ];
        argv.extend(perms.args);
        if !prompt.trim().is_empty() {
            argv.extend(["--system".to_string(), prompt.to_string()]);
        }
        argv.push(
            self.normalize_model(model)
                .unwrap_or_else(|| LEMONADE_DEFAULT_CLI_MODEL.to_string()),
        );
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OllamaCliAdapter;

impl VendorCliAdapter for OllamaCliAdapter {
    fn vendor_id(&self) -> &str {
        "ollama"
    }

    fn binary_env_var(&self) -> Option<&str> {
        Some("FL_WIRE_OLLAMA_BIN")
    }

    fn default_binary(&self) -> &str {
        "ollama"
    }

    fn normalize_model(&self, requested: Option<&str>) -> Option<String> {
        requested
            .map(str::trim)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 64
                    && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                    && id.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/')
                    })
            })
            .map(str::to_string)
            .or_else(|| Some(OLLAMA_DEFAULT_CLI_MODEL.to_string()))
    }

    fn build_launch_spec_with_options(
        &self,
        persona: &str,
        model: Option<&str>,
        _thinking_level: Option<&str>,
        prompt: &str,
        home: Option<&Path>,
        options: VendorLaunchOptions,
    ) -> Result<LaunchSpec, String> {
        let perms = self.check_posture(options.posture)?;
        let mut argv = vec![
            self.resolve_binary(home),
            "run".to_string(),
            self.normalize_model(model)
                .unwrap_or_else(|| OLLAMA_DEFAULT_CLI_MODEL.to_string()),
        ];
        argv.extend(perms.args);
        if !prompt.trim().is_empty() {
            argv.push(muse_prompt_argument(prompt));
        }
        Ok(LaunchSpec {
            env: launch_env_for_vendor(self.vendor_id(), persona, home),
            argv,
        })
    }
}

// ---------------------------------------------------------------------------
// HostVendorAdapterRegistry
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HostVendorAdapterRegistry {
    adapters: RwLock<BTreeMap<String, Arc<dyn VendorCliAdapter>>>,
}

impl Default for HostVendorAdapterRegistry {
    fn default() -> Self {
        let mut map: BTreeMap<String, Arc<dyn VendorCliAdapter>> = BTreeMap::new();
        let builtins: [Arc<dyn VendorCliAdapter>; 7] = [
            Arc::new(ClaudeCliAdapter),
            Arc::new(CodexCliAdapter),
            Arc::new(AntigravityCliAdapter),
            Arc::new(GrokCliAdapter),
            Arc::new(MuseCliAdapter),
            Arc::new(LemonadeCliAdapter),
            Arc::new(OllamaCliAdapter),
        ];
        for adapter in builtins {
            map.insert(adapter.vendor_id().to_string(), adapter);
        }
        Self {
            adapters: RwLock::new(map),
        }
    }
}

impl HostVendorAdapterRegistry {
    pub fn global() -> &'static Self {
        static REGISTRY: OnceLock<HostVendorAdapterRegistry> = OnceLock::new();
        REGISTRY.get_or_init(Self::default)
    }

    pub fn lookup(&self, vendor: &str) -> Option<Arc<dyn VendorCliAdapter>> {
        let key = vendor.trim().to_ascii_lowercase();
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    pub fn register(&self, adapter: Arc<dyn VendorCliAdapter>) {
        let key = adapter.vendor_id().trim().to_ascii_lowercase();
        self.adapters
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, adapter);
    }

    pub fn list(&self) -> Vec<Arc<dyn VendorCliAdapter>> {
        self.adapters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

pub fn lookup_vendor_cli_adapter(vendor: &str) -> Option<Arc<dyn VendorCliAdapter>> {
    HostVendorAdapterRegistry::global().lookup(vendor)
}

pub fn register_vendor_cli_adapter(adapter: Arc<dyn VendorCliAdapter>) {
    HostVendorAdapterRegistry::global().register(adapter);
}

pub fn list_vendor_cli_adapters() -> Vec<Arc<dyn VendorCliAdapter>> {
    HostVendorAdapterRegistry::global().list()
}

/// Default binary names (lowercased file names) of all registered CLI adapters.
pub fn vendor_cli_binaries() -> Vec<String> {
    let mut bins: Vec<String> = list_vendor_cli_adapters()
        .into_iter()
        .filter_map(|a| {
            Path::new(a.default_binary())
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
        })
        .collect();
    bins.sort();
    bins.dedup();
    bins
}

// ---------------------------------------------------------------------------
// Deterministic unit <-> SessionId mapping helpers
// ---------------------------------------------------------------------------

/// Map a runtime unit or target (`"Ada:claude"`, `"muse:ada"`, `"Ada"`) onto a valid
/// `fl_host_proto::SessionId` (`"Ada.claude"`, `"muse.ada"`, `"Ada"`).
pub fn unit_to_host_session_id(unit: &str) -> Option<SessionId> {
    let trimmed = unit.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace(':', ".");
    SessionId::new(&normalized).ok()
}

/// Map a `fl_host_proto::SessionId` (`"Ada.claude"`, `"muse.ada"`, `"Ada"`) back to its
/// `session:window` or bare session string (`"Ada:claude"`, `"muse:ada"`, `"Ada"`).
pub fn host_session_id_to_unit(id: &SessionId) -> String {
    match id.as_str().split_once('.') {
        Some((session, window)) => format!("{session}:{window}"),
        None => id.as_str().to_string(),
    }
}

/// Live `SessionId`s in `sessions` that belong to `unit`.
///
/// - `unit == ""` or `unit == "muse"` matches nothing (the shared Muse container is never a single
///   session unit).
/// - `unit` containing `:` (`"muse:ada"`, `"Ada:claude"`) matches the exact `SessionId`
///   (`"muse.ada"`, `"Ada.claude"`).
/// - Bare `unit` (`"Ada"`) matches `"Ada"` and any `"Ada.<window>"` (`"Ada.claude"`, etc.).
pub fn matching_live_host_sessions(sessions: &[SessionSummary], unit: &str) -> Vec<SessionId> {
    let unit = unit.trim();
    if unit.is_empty() || unit == "muse" {
        return Vec::new();
    }
    let live = sessions
        .iter()
        .filter(|s| matches!(s.state, ChildState::Running(_)));
    if unit.contains(':') {
        let Some(wanted) = unit_to_host_session_id(unit) else {
            return Vec::new();
        };
        return live
            .filter(|s| s.stream.session == wanted)
            .map(|s| s.stream.session.clone())
            .collect();
    }
    live.filter(|s| {
        let id = s.stream.session.as_str();
        id == unit
            || id
                .strip_prefix(unit)
                .is_some_and(|rest| rest.starts_with('.'))
    })
    .map(|s| s.stream.session.clone())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{
        ChildRecord, HostInstance, InputState, SessionEpoch, SessionMode, SpawnKind, StreamId,
    };

    fn summary(id: &str, running: bool) -> SessionSummary {
        let session = SessionId::new(id).unwrap();
        let record = ChildRecord {
            session: session.clone(),
            session_epoch: SessionEpoch(1),
            kind: SpawnKind::Pipe,
            pid: 42,
            input: InputState::Writable,
            started_at_ms: 1,
            exit: None,
            consumer_stalled: false,
        };
        SessionSummary {
            stream: StreamId {
                host: HostInstance::new([1u8; 16]),
                session,
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::ReliablePipe,
            state: if running {
                ChildState::Running(record)
            } else {
                ChildState::CleanupComplete(record)
            },
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        }
    }

    #[test]
    fn registry_contains_all_seven_builtin_vendor_cli_adapters() {
        let ids: Vec<String> = list_vendor_cli_adapters()
            .into_iter()
            .map(|a| a.vendor_id().to_string())
            .collect();
        for expected in [
            "antigravity",
            "claude",
            "codex",
            "grok",
            "lemonade",
            "muse",
            "ollama",
        ] {
            assert!(ids.contains(&expected.to_string()), "missing {expected}");
            assert!(lookup_vendor_cli_adapter(expected).is_some());
        }
        let bins = vendor_cli_binaries();
        for bin in [
            "agy", "claude", "codex", "grok", "lemonade", "muse", "ollama",
        ] {
            assert!(bins.contains(&bin.to_string()), "missing bin {bin}");
        }
    }

    #[test]
    fn thinking_level_reaches_claude_codex_and_agy_launch_argv() {
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        let spec = claude.build_launch_spec("ada", Some("opus"), Some("high"), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--model",
                "opus",
                "--effort",
                "high",
                "Hi"
            ]
        );
        let spec = claude.build_launch_spec("ada", None, Some(" max "), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--effort",
                "max",
                "Hi"
            ]
        );
        // When both model and thinking_level are None, `--permission-mode` must still sit
        // between `--remote-control` and the positional prompt so Commander.js (`--remote-control [name]`)
        // never swallows the prompt as the session name.
        let spec = claude.build_launch_spec("ada", None, None, "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &["--remote-control", "--permission-mode", "auto", "Hi"]
        );

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), Some("xhigh"), "Hi", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "-a",
                "on-request",
                "-s",
                "workspace-write",
                "--remote",
                "unix://",
                "--model",
                "gpt-5",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "Hi"
            ]
        );

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        let spec = agy.build_launch_spec(
            "ada",
            Some("claude-opus-4-6-thinking"),
            Some("high"),
            "Hi",
            None,
        );
        assert!(
            spec.argv
                .windows(2)
                .any(|w| w == ["--model", "claude-opus-4-6-thinking-high"]),
            "{:?}",
            spec.argv
        );

        // Vendors with no launch option for it ignore the level.
        for vendor in ["grok", "muse", "lemonade", "ollama"] {
            let a = lookup_vendor_cli_adapter(vendor).unwrap();
            let with = a.build_launch_spec("ada", None, Some("high"), "Hi", None);
            let without = a.build_launch_spec("ada", None, None, "Hi", None);
            assert_eq!(with.argv, without.argv, "{vendor}");
        }
    }

    #[test]
    fn remote_control_is_configurable_at_the_adapter_layer() {
        let off = VendorLaunchOptions {
            remote_control: false,
            posture: Posture::Auto,
        };
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        assert!(claude.supports_remote_control());
        let spec_off = claude
            .build_launch_spec_with_options("ada", Some("opus"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &["--permission-mode", "auto", "--model", "opus", "Hi"]
        );

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        assert!(codex.supports_remote_control());
        let spec_off = codex
            .build_launch_spec_with_options("ada", Some("gpt-5"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &[
                "-a",
                "on-request",
                "-s",
                "workspace-write",
                "--model",
                "gpt-5",
                "Hi"
            ]
        );

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        assert!(agy.supports_remote_control());
        let spec_off = agy
            .build_launch_spec_with_options("ada", Some("claude-sonnet-5-5"), None, "Hi", None, off)
            .unwrap();
        assert_eq!(
            &spec_off.argv[1..],
            &[
                "--mode",
                "auto",
                "--model",
                "claude-sonnet-5-5-medium",
                "--prompt-interactive",
                "Hi"
            ]
        );
    }

    #[test]
    fn hostile_or_empty_thinking_levels_never_reach_argv() {
        for bad in [
            "",
            "   ",
            "high; rm -rf /",
            "hi\"gh",
            "hi'gh",
            "high\nlow",
            "--model",
            "high low",
            "$(id)",
        ] {
            for vendor in ["claude", "codex", "antigravity"] {
                let a = lookup_vendor_cli_adapter(vendor).unwrap();
                let with = a.build_launch_spec("ada", Some("opus"), Some(bad), "Hi", None);
                let without = a.build_launch_spec("ada", Some("opus"), None, "Hi", None);
                assert_eq!(with.argv, without.argv, "{vendor} {bad:?}");
            }
        }
        assert_eq!(plain_thinking_level(Some("high")), Some("high"));
        assert_eq!(plain_thinking_level(Some("-x")), None);
    }

    #[test]
    fn seven_vendor_launch_specs_and_controls_match_contracts() {
        let claude = lookup_vendor_cli_adapter("claude").unwrap();
        let spec = claude.build_launch_spec("ada", Some("opus[1m]"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--model",
                "opus[1m]",
                "Hello"
            ]
        );
        assert_eq!(claude.interrupt_key(), "Escape");
        assert_eq!(claude.reset_command(), "/clear");
        assert_eq!(claude.startup_prompt_rules().len(), 1);

        let codex = lookup_vendor_cli_adapter("codex").unwrap();
        let spec = codex.build_launch_spec("ada", Some("gpt-5"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "-a",
                "on-request",
                "-s",
                "workspace-write",
                "--remote",
                "unix://",
                "--model",
                "gpt-5",
                "Hello"
            ]
        );
        assert!(spec.env.iter().all(|(k, _)| k != "AGENT_PERSONA"));
        assert_eq!(codex.interrupt_key(), "Escape");

        let agy = lookup_vendor_cli_adapter("antigravity").unwrap();
        let spec = agy.build_launch_spec("ada", Some("claude-sonnet-5-5"), None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--mode",
                "auto",
                "--remote-control",
                "--model",
                "claude-sonnet-5-5-medium",
                "--prompt-interactive",
                "Hello"
            ]
        );
        assert_eq!(agy.interrupt_key(), "C-c");
        assert_eq!(agy.startup_prompt_rules().len(), 1);
        assert_eq!(
            agy.startup_prompt_rules()[0].match_text,
            AGY_PROJECT_TRUST_PROMPT
        );
        assert_eq!(agy.startup_prompt_rules()[0].response_keys, "Enter");
        assert!(!agy.startup_prompt_rules()[0].warn_on_miss);
        for alias in [
            "argon-4.0",
            "argon-4.0-medium",
            "argon-4",
            "argon",
            "gemini-4.0-argon",
            "gemini-4-argon",
        ] {
            assert_eq!(agy_cli_model_id(alias, None), "gemini-4-argon-medium");
            assert_eq!(agy_cli_model_id(alias, Some("high")), "gemini-4-argon-high");
        }

        let grok = lookup_vendor_cli_adapter("grok").unwrap();
        let spec = grok.build_launch_spec("ada", Some("grok-4"), None, "-start", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--permission-mode",
                "auto",
                "--model",
                "grok-4",
                "Prompt: -start"
            ]
        );
        assert_eq!(grok.startup_prompt_rules().len(), 1);
        assert!(grok.startup_prompt_rules()[0].warn_on_miss);

        let muse = lookup_vendor_cli_adapter("muse").unwrap();
        let spec = muse.build_launch_spec("ada", None, None, "Hello", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--model",
                MUSE_DEFAULT_MODEL,
                "--approval-mode",
                "on-request",
                "--approval-judge",
                "off",
                "Hello"
            ]
        );
        assert_eq!(muse.reset_command(), "/new");

        let lemonade = lookup_vendor_cli_adapter("lemonade").unwrap();
        let spec = lemonade.build_launch_spec("ada", None, None, "System prompt", None);
        assert_eq!(
            &spec.argv[1..],
            &[
                "--no-discovery",
                "--host",
                "127.0.0.1",
                "--port",
                "13305",
                "chat",
                "--system",
                "System prompt",
                LEMONADE_DEFAULT_CLI_MODEL
            ]
        );

        let ollama = lookup_vendor_cli_adapter("ollama").unwrap();
        let spec = ollama.build_launch_spec("ada", None, None, "Hello ollama", None);
        assert_eq!(
            &spec.argv[1..],
            &["run", OLLAMA_DEFAULT_CLI_MODEL, "Hello ollama"]
        );
    }

    #[test]
    fn unit_and_session_id_round_trip_and_match_live_sessions() {
        let id = unit_to_host_session_id("Ada:claude").unwrap();
        assert_eq!(id.as_str(), "Ada.claude");
        assert_eq!(host_session_id_to_unit(&id), "Ada:claude");

        let sessions = vec![
            summary("Ada.claude", true),
            summary("muse.ada", true),
            summary("muse.bob", false),
            summary("Ada2.claude", true),
        ];
        assert_eq!(
            matching_live_host_sessions(&sessions, "Ada"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "Ada:claude"),
            vec![SessionId::new("Ada.claude").unwrap()]
        );
        assert_eq!(
            matching_live_host_sessions(&sessions, "muse:ada"),
            vec![SessionId::new("muse.ada").unwrap()]
        );
        assert!(matching_live_host_sessions(&sessions, "muse:bob").is_empty());
        assert!(matching_live_host_sessions(&sessions, "muse").is_empty());
    }

    #[test]
    fn vendor_posture_matrix_emits_bypass_only_on_bypass_and_refuses_unsupported() {
        const BYPASS_FLAGS: [&str; 7] = [
            "--yolo",
            "--disable-approval",
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--dangerously-bypass-approvals-and-sandbox",
            "--always-approve",
            "bypassPermissions",
        ];
        let has_bypass = |argv: &[String]| {
            argv.iter().any(|a| BYPASS_FLAGS.contains(&a.as_str()))
                || argv.windows(2).any(|w| w == ["-a", "never"])
        };

        let all_postures = [
            Posture::Ask,
            Posture::AcceptEdits,
            Posture::Auto,
            Posture::Plan,
            Posture::Bypass,
        ];
        for vendor in [
            "claude",
            "codex",
            "antigravity",
            "grok",
            "muse",
            "lemonade",
            "ollama",
        ] {
            let adapter = lookup_vendor_cli_adapter(vendor).unwrap();
            for posture in all_postures {
                let res = adapter.check_posture(posture);
                let unsupported = matches!(
                    (vendor, posture),
                    ("codex", Posture::Ask)
                        | ("antigravity", Posture::Ask)
                        | ("muse", Posture::Plan)
                ) || (matches!(vendor, "lemonade" | "ollama")
                    && posture != Posture::Auto);
                let spec_res = adapter.build_launch_spec_with_options(
                    "ada",
                    None,
                    None,
                    "Hydrate",
                    None,
                    VendorLaunchOptions {
                        remote_control: true,
                        posture,
                    },
                );
                if unsupported {
                    let err = res
                        .err()
                        .unwrap_or_else(|| panic!("{vendor} {posture} must be refused"));
                    assert!(!err.is_empty(), "{vendor} {posture}");
                    let spec_err = spec_res.err().unwrap_or_else(|| {
                        panic!("{vendor} {posture} launch spec must fail closed")
                    });
                    assert_eq!(spec_err, err, "{vendor} {posture}");
                    continue;
                }
                let perms = res
                    .unwrap_or_else(|e| panic!("{vendor} {posture} must be supported, got: {e}"));
                assert!(!perms.receipts.is_empty(), "{vendor} {posture}");
                for receipt in &perms.receipts {
                    assert!(!receipt.summary.is_empty(), "{vendor} {posture}");
                }
                let spec = spec_res.unwrap_or_else(|e| {
                    panic!("{vendor} {posture} launch spec must succeed, got: {e}")
                });
                assert_eq!(
                    has_bypass(&spec.argv),
                    posture == Posture::Bypass && !matches!(vendor, "lemonade" | "ollama"),
                    "{vendor} {posture}: {:?}",
                    spec.argv
                );
                assert!(
                    !spec
                        .argv
                        .iter()
                        .any(|a| a == "--yolo" || a == "--always-approve"),
                    "{vendor} {posture} must never emit --yolo or --always-approve: {:?}",
                    spec.argv
                );
            }
        }
    }
}
