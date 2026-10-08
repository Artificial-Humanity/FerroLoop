//! Freezing a registry entry into a catalog entry (MCP spec §3.2): the launch
//! spec is written out in full when a server is added, so `sync` reads nothing
//! else. What fl cannot freeze honestly is refused, naming what to do instead.
//! And what `upgrade` shows and checks (MCP spec §3.3). Nothing here reads the
//! network: these functions take what the registry client read.

use crate::McpError;
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, is_env_name, is_variable_name};
use crate::registry::{
    Argument, ArgumentKind, Input, KeyValueInput, Package, RegistryType, Remote, ServerJson,
    ServerResponse, Status, TransportKind, has_unseen, printable,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A launch route, as `add` names it: `--package npm|pypi|oci` or `--remote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Npm,
    Pypi,
    Oci,
    Remote,
}

impl Route {
    /// The flag that chooses it.
    pub fn flag(self) -> &'static str {
        match self {
            Route::Npm => "--package npm",
            Route::Pypi => "--package pypi",
            Route::Oci => "--package oci",
            Route::Remote => "--remote",
        }
    }

    /// The route a frozen entry was taken from, read from its command or its
    /// transport; `None` for a command fl does not freeze to.
    pub fn of(server: &Server) -> Option<Route> {
        match server.transport {
            Transport::Http | Transport::Sse => Some(Route::Remote),
            Transport::Stdio => match server.command.as_deref() {
                Some("npx") => Some(Route::Npm),
                Some("uvx") => Some(Route::Pypi),
                Some("docker") => Some(Route::Oci),
                _ => None,
            },
        }
    }
}

/// What `add --from` (or `upgrade`) asks of the freeze.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeOptions {
    /// The catalog name: it names the variables fl derives for secret
    /// headers, and the commands a refusal suggests.
    pub name: String,
    /// `None`: the only route the entry offers.
    pub route: Option<Route>,
    /// `--env NAME=value`: a value for a non-secret environment variable,
    /// recorded as a literal. Each must name one of the package's.
    pub env: BTreeMap<String, String>,
    /// `--with NAME`: an optional argument or environment variable that
    /// needs a secret, included. Each must name one.
    pub with: BTreeSet<String>,
    /// From the pinned entry, on `upgrade`: its literal values and the
    /// secrets it included, used where they still apply and ignored where
    /// they do not. `env` and `with` win over them.
    pub kept_env: BTreeMap<String, String>,
    pub kept_with: BTreeSet<String>,
    /// Set by [`FreezeOptions::upgrading`]: the freeze is an `upgrade`, which
    /// takes no `--package` or `--remote`, so a refusal about the route
    /// names the commands that do.
    pub upgrading: bool,
}

impl FreezeOptions {
    /// The options `upgrade` freezes the new version with: the pinned entry's
    /// route, literal values and included secrets.
    pub fn upgrading(name: &str, pinned: &Server) -> FreezeOptions {
        let env = pinned.env.iter().flatten();
        let kept_env = env
            .clone()
            .filter_map(|(k, v)| match v {
                EnvValue::Literal(value) => Some((k.clone(), value.clone())),
                EnvValue::Secret { .. } => None,
            })
            .collect();
        // An included `-e NAME={var}` became a secret `NAME`; an included
        // optional variable is a secret `NAME` too.
        let kept_with = env
            .filter(|(_, v)| matches!(v, EnvValue::Secret { .. }))
            .map(|(k, _)| k.clone())
            .collect();
        FreezeOptions {
            name: name.to_string(),
            route: Route::of(pinned),
            kept_env,
            kept_with,
            upgrading: true,
            ..FreezeOptions::default()
        }
    }
}

/// A frozen entry, and what `add` prints about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frozen {
    pub server: Server,
    /// A deprecated status, and every literal value the entry records: each
    /// is committed with the catalog, public if the repository is (MCP spec
    /// §6).
    pub warnings: Vec<String>,
    /// The variables a person sets before a session starts the server,
    /// sorted.
    pub secrets: Vec<String>,
    /// Optional arguments left out, and how to include each.
    pub notes: Vec<String>,
}

/// Freezes `resp` into a catalog entry by the route `opts` names, or by the
/// only route it offers. Refused: [`McpError::Unfreezable`], naming what to
/// do instead. No secret value enters the entry or a message.
pub fn freeze(resp: &ServerResponse, opts: &FreezeOptions) -> Result<Frozen, McpError> {
    freeze_entry(resp, opts).map_err(|r| McpError::Unfreezable {
        from: printable(&resp.server.name),
        version: printable(&resp.server.version),
        problem: r.problem,
        next: r.next,
    })
}

/// Why an entry cannot be frozen, and what to do instead.
struct Refusal {
    problem: String,
    next: String,
}

fn refuse<T>(problem: impl Into<String>, next: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        problem: problem.into(),
        next: next.into(),
    })
}

fn by_hand(name: &str) -> String {
    format!("Add it by hand with `fl mcp add {name} -- <command>`")
}

fn by_hand_url(name: &str) -> String {
    format!("Add it by hand with `fl mcp add {name} --url <url>`")
}

fn freeze_entry(resp: &ServerResponse, opts: &FreezeOptions) -> Result<Frozen, Refusal> {
    let s = &resp.server;
    let n = &opts.name;
    let mut warnings = Vec::new();
    let said = || match &resp.meta.status_message {
        Some(m) => printable(m),
        None => "no reason given".to_string(),
    };
    match &resp.meta.status {
        Status::Deleted => {
            return refuse(
                format!("it is deleted in the registry ({})", said()),
                "Choose another server: `fl mcp search <text>` lists them",
            );
        }
        Status::Deprecated => warnings.push(format!(
            "`{}` {} is deprecated in the registry ({})",
            printable(&s.name),
            printable(&s.version),
            said()
        )),
        Status::Active => {}
        Status::Other(status) => {
            return refuse(
                format!(
                    "its status in the registry is `{}`, one fl does not know, so it cannot \
                     tell whether the version may be used",
                    printable(status)
                ),
                format!(
                    "Choose another version or server: `fl mcp search <text>` lists them. Or add \
                     it by hand with `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
                ),
            );
        }
    }
    if has_unseen(&s.name) {
        return unseen("its server name", n);
    }
    if has_unseen(&s.version) {
        return unseen("its version", n);
    }
    if !is_exact(&s.version) {
        return refuse(
            format!(
                "its version `{}` is not an exact version",
                printable(&s.version)
            ),
            format!(
                "The registry gives no version to pin; add the server by hand with \
                 `fl mcp add {n} -- <command>`"
            ),
        );
    }
    let mut b = Build {
        opts,
        env: BTreeMap::new(),
        env_names: BTreeSet::new(),
        with_keys: BTreeSet::new(),
        notes: Vec::new(),
    };
    let launch = match choose(s, opts)? {
        Offer::Package(p) => b.package(p)?,
        Offer::Remote(r) => b.remote(r)?,
    };
    for k in opts.env.keys() {
        if !b.env_names.contains(k) {
            // A key that is not a variable's name may be a token pasted by
            // mistake: it is not repeated (MCP spec §6).
            let problem = match is_variable_name(k) {
                true => format!("`--env {k}=…` names no environment variable of this launch route"),
                false => "an `--env` key that is not a variable's name names no environment \
                          variable of this launch route, and fl does not repeat it"
                    .to_string(),
            };
            return refuse(
                problem,
                "Drop it, or check its name against the registry entry",
            );
        }
    }
    for k in &opts.with {
        if !b.with_keys.contains(k) {
            let problem = match is_variable_name(k) {
                true => {
                    format!(
                        "`--with {k}` names no optional argument or variable that needs a secret"
                    )
                }
                false => "a `--with` name that is not a variable's name names no optional \
                          argument or variable that needs a secret, and fl does not repeat it"
                    .to_string(),
            };
            return refuse(
                problem,
                "Drop it, or check its name against the registry entry",
            );
        }
    }
    let server = Server {
        from: Some(s.name.clone()),
        version: Some(s.version.clone()),
        enabled: true,
        vendors: None,
        env: (!b.env.is_empty()).then_some(b.env),
        ..launch
    };
    if let Some(what) = unseen_field(&server) {
        return unseen(&what, n);
    }
    let secret_headers: Vec<&String> = (server.headers.iter().flatten())
        .filter_map(|(_, v)| match v {
            HeaderValue::Secret { env, .. } => Some(env),
            HeaderValue::Literal(_) => None,
        })
        .collect();
    let names = server.env.iter().flatten().map(|(k, _)| k);
    if let Some(bad) = names
        .chain(secret_headers.iter().copied())
        .find(|v| !is_env_name(v))
    {
        return refuse(
            format!(
                "`{}` cannot be an environment variable name",
                printable(bad)
            ),
            format!(
                "fl cannot pass it to the server; add the server by hand with \
                 `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
            ),
        );
    }
    let secrets = server
        .secret_vars()
        .into_iter()
        .map(str::to_string)
        .collect();
    warnings.extend(literal_warnings(&server));
    Ok(Frozen {
        server,
        warnings,
        secrets,
        notes: b.notes,
    })
}

/// A version fl may pin: not empty, and not the word `latest`.
fn is_exact(version: &str) -> bool {
    !version.is_empty() && version != "latest"
}

#[derive(Clone, Copy)]
enum Offer<'a> {
    Package(&'a Package),
    Remote(&'a Remote),
}

impl Offer<'_> {
    fn route(self) -> Option<Route> {
        match self {
            Offer::Package(p) => match p.registry_type {
                RegistryType::Npm => Some(Route::Npm),
                RegistryType::Pypi => Some(Route::Pypi),
                RegistryType::Oci => Some(Route::Oci),
                RegistryType::Other(_) => None,
            },
            Offer::Remote(_) => Some(Route::Remote),
        }
    }

    fn label(self) -> String {
        match (self, self.route()) {
            (Offer::Package(p), Some(route)) => {
                format!("`{}` ({})", route.flag(), printable(&p.identifier))
            }
            (Offer::Package(p), None) => format!(
                "a `{}` package, which fl cannot freeze",
                printable(p.registry_type.as_str())
            ),
            (Offer::Remote(r), _) => format!(
                "`--remote` ({}, {})",
                printable(r.kind.as_str()),
                printable(&r.url)
            ),
        }
    }
}

/// MCP spec §3.2 step 3: the route named, or the only one on offer.
fn choose<'a>(s: &'a ServerJson, opts: &FreezeOptions) -> Result<Offer<'a>, Refusal> {
    let n = &opts.name;
    let offers: Vec<Offer> = (s.packages.iter().map(Offer::Package))
        .chain(s.remotes.iter().map(Offer::Remote))
        .collect();
    let list = || {
        let labels: Vec<String> = offers.iter().map(|o| o.label()).collect();
        labels.join(", ")
    };
    if offers.is_empty() {
        return refuse(
            "it offers no launch route: no package and no remote",
            format!(
                "Add it by hand with `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
            ),
        );
    }
    let Some(route) = opts.route else {
        if let [only] = offers.as_slice() {
            return Ok(*only);
        }
        return refuse(
            format!("it offers more than one launch route: {}", list()),
            "Choose one with `--package <type>` or `--remote`",
        );
    };
    let mut hits = offers.iter().filter(|o| o.route() == Some(route));
    match (hits.next(), hits.next()) {
        (Some(only), None) => Ok(*only),
        (None, _) => refuse(
            format!("it offers no `{}` route, only {}", route.flag(), list()),
            if opts.upgrading {
                // `upgrade` has no `--package` or `--remote`: the entry is
                // removed and added again by the route the new version offers.
                format!(
                    "`fl mcp upgrade` keeps the route the entry was frozen from. Take the route \
                     the new version offers: `fl mcp remove {n}`, then `fl mcp add {n} --from {} \
                     --package <type>` or `--remote`",
                    printable(&s.name)
                )
            } else {
                "Choose one it offers".to_string()
            },
        ),
        (Some(_), Some(_)) => refuse(
            format!(
                "it offers the `{}` route more than once: {}",
                route.flag(),
                list()
            ),
            match route {
                Route::Remote => by_hand_url(n),
                _ => by_hand(n),
            },
        ),
    }
}

/// The parts of an entry one route builds.
struct Build<'a> {
    opts: &'a FreezeOptions,
    env: BTreeMap<String, EnvValue>,
    /// The package's environment variables, which `--env` may name.
    env_names: BTreeSet<String>,
    /// The optional arguments that need a secret, which `--with` may name.
    with_keys: BTreeSet<String>,
    notes: Vec<String>,
}

impl Build<'_> {
    /// MCP spec §3.2 steps 4 and 6: a package fl can run as it is pinned,
    /// over stdio, or a refusal.
    fn package(&mut self, p: &Package) -> Result<Server, Refusal> {
        let n = &self.opts.name;
        let id = printable(&p.identifier);
        if has_unseen(&p.identifier) {
            return unseen("its package identifier", n);
        }
        if p.version.as_deref().is_some_and(has_unseen) {
            return unseen("its package version", n);
        }
        if let RegistryType::Other(kind) = &p.registry_type {
            return refuse(
                format!(
                    "it is a `{}` package; fl freezes npm, PyPI and OCI packages only",
                    printable(kind)
                ),
                by_hand(n),
            );
        }
        if p.transport.kind != TransportKind::Stdio {
            return refuse(
                format!(
                    "its package's transport is `{}`, not stdio: it serves HTTP on this \
                     machine, and fl does not start servers",
                    printable(p.transport.kind.as_str())
                ),
                format!("Start it yourself, then add it by hand with `fl mcp add {n} --url <url>`"),
            );
        }
        let docker = p.registry_type == RegistryType::Oci;
        let (command, mut args, pinned) = if docker {
            if let Err(problem) = image_pin(&p.identifier) {
                return refuse(
                    problem,
                    format!(
                        "Add it by hand with `fl mcp add {n} -- docker run -i --rm <image>`, \
                         naming an exact image"
                    ),
                );
            }
            let args = strings(&["run", "-i", "--rm"]);
            ("docker", args, p.identifier.clone())
        } else {
            let version = p.version.as_deref().unwrap_or_default();
            if !is_exact(version) {
                let gives = match version {
                    "" => "none".to_string(),
                    v => format!("`{}`", printable(v)),
                };
                return refuse(
                    format!("its package `{id}` names no exact version (it gives {gives})"),
                    by_hand(n),
                );
            }
            let (kind, name_ok, version_ok) = match p.registry_type {
                RegistryType::Npm => ("npm", is_npm_name(&p.identifier), is_npm_version(version)),
                _ => (
                    "PyPI",
                    is_pypi_name(&p.identifier),
                    is_pypi_version(version),
                ),
            };
            if !name_ok {
                return refuse(
                    format!("its package name `{id}` is not a valid {kind} package name"),
                    by_hand(n),
                );
            }
            if !version_ok {
                return refuse(
                    format!(
                        "its package `{id}` has the version `{}`, which is not an exact {kind} \
                         version",
                        printable(version)
                    ),
                    by_hand(n),
                );
            }
            match p.registry_type {
                RegistryType::Npm => {
                    let pinned = format!("{}@{version}", p.identifier);
                    ("npx", strings(&["-y"]), pinned)
                }
                _ => ("uvx", Vec::new(), format!("{}=={version}", p.identifier)),
            }
        };
        for a in &p.runtime_arguments {
            if docker && a.kind == ArgumentKind::Positional {
                return refuse(
                    format!(
                        "its OCI package has a positional runtime argument (`{}`), so fl \
                         cannot tell its `docker run` line from the one it would write",
                        label(a)
                    ),
                    by_hand(n),
                );
            }
            args.extend(self.argument(a, docker)?);
        }
        for v in &p.environment_variables {
            if self.variable(v)? && docker {
                args.extend(["-e".to_string(), v.name.clone()]);
            }
        }
        args.push(pinned);
        for a in &p.package_arguments {
            args.extend(self.argument(a, false)?);
        }
        Ok(Server {
            from: None,
            version: None,
            enabled: true,
            vendors: None,
            transport: Transport::Stdio,
            command: Some(command.to_string()),
            args: Some(args),
            env: None,
            url: None,
            headers: None,
        })
    }

    /// One argument as it is passed: a named one as `<name> <value>`, a named
    /// boolean as `<name>` or nothing, a positional as its value; or none. A
    /// secret only as docker's `-e NAME`, and an optional one only when named
    /// with `--with`: an agent passes an unset `${NAME}` on as text.
    fn argument(&mut self, a: &Argument, docker: bool) -> Result<Vec<String>, Refusal> {
        let n = &self.opts.name;
        let label = label(a);
        // Refused before anything else is read of it, optional or not: fl
        // cannot tell how such an argument is passed (the registry serves
        // types its own schema does not define).
        if let ArgumentKind::Other(kind) = &a.kind {
            return refuse(
                format!(
                    "its argument `{label}` is of type `{}`, an argument type fl does not know",
                    printable(kind)
                ),
                by_hand(n),
            );
        }
        let template = a.value.as_deref().or(a.default.as_deref());
        let secret = a.is_secret
            || template.is_some_and(|t| names_in(t, &a.variables).any(|(_, i)| i.is_secret));
        if secret {
            let shape = env_shape(a);
            let key = shape.clone().unwrap_or_else(|| label.clone());
            if !a.is_required {
                self.with_keys.insert(key.clone());
                if !self.opts.with.contains(&key) && !self.opts.kept_with.contains(&key) {
                    self.notes.push(format!(
                        "Left out `{label}`: it is optional and needs a secret. \
                         `--with {key}` includes it"
                    ));
                    return Ok(Vec::new());
                }
            }
            if docker && let Some(name) = shape {
                self.env
                    .insert(name.clone(), EnvValue::Secret { env: None });
                return Ok(vec![a.name.clone().unwrap_or_default(), name]);
            }
            return refuse(
                format!(
                    "its argument `{label}` holds a secret, which fl would have to commit; a \
                     secret in an argument is taken only as docker's `-e NAME={{var}}`"
                ),
                format!(
                    "Add it by hand with `fl mcp add {n} -- <command>`, passing the secret in an \
                     environment variable"
                ),
            );
        }
        let Some(template) = template else {
            if a.is_required {
                return refuse(
                    format!("its argument `{label}` is required and has no value"),
                    by_hand(n),
                );
            }
            return Ok(Vec::new());
        };
        let value = match resolve(template, &a.variables) {
            Ok(value) => value,
            Err(var) if a.is_required => {
                return refuse(
                    format!("its argument `{label}` names an unset variable, `{{{var}}}`"),
                    by_hand(n),
                );
            }
            Err(_) => return Ok(Vec::new()),
        };
        let name = a.name.clone().unwrap_or_default();
        Ok(match (&a.kind, a.format.as_deref(), value.as_str()) {
            (ArgumentKind::Positional, ..) => vec![value],
            (ArgumentKind::Named, Some("boolean"), "true") => vec![name],
            (ArgumentKind::Named, Some("boolean"), "false") => Vec::new(),
            (ArgumentKind::Named, ..) => vec![name, value],
            (ArgumentKind::Other(_), ..) => unreachable!("refused above"),
        })
    }

    /// One environment variable: a secret reference, or a literal from
    /// `--env`, the pinned entry, or the registry. Whether it is passed. An
    /// optional secret is left out unless named with `--with`, as an optional
    /// secret argument is: an agent passes an unset `${NAME}` on as text.
    fn variable(&mut self, v: &KeyValueInput) -> Result<bool, Refusal> {
        let name = &v.name;
        let shown = printable(name);
        self.env_names.insert(name.clone());
        let template = v.value.as_deref().or(v.default.as_deref());
        let secret = v.is_secret
            || template.is_some_and(|t| names_in(t, &v.variables).any(|(_, i)| i.is_secret));
        if secret {
            if self.opts.env.contains_key(name) {
                return refuse(
                    format!("`--env {shown}=…` names a secret, and a secret is never recorded"),
                    format!("Set {shown} in the environment instead"),
                );
            }
            if !v.is_required {
                self.with_keys.insert(name.clone());
                if !self.opts.with.contains(name) && !self.opts.kept_with.contains(name) {
                    self.notes.push(format!(
                        "Left out `{shown}`: it is optional and needs a secret. \
                         `--with {shown}` includes it"
                    ));
                    return Ok(false);
                }
            }
            self.env
                .insert(name.clone(), EnvValue::Secret { env: None });
            return Ok(true);
        }
        let given = (self.opts.env.get(name))
            .or(self.opts.kept_env.get(name))
            .cloned();
        let value = given.or_else(|| template.and_then(|t| resolve(t, &v.variables).ok()));
        match value {
            Some(value) => {
                self.env.insert(name.clone(), EnvValue::Literal(value));
                Ok(true)
            }
            None if v.is_required => refuse(
                format!("its environment variable `{shown}` needs a value"),
                format!(
                    "Give it with `--env {shown}=<value>`; the value is committed with the catalog"
                ),
            ),
            None => Ok(false),
        }
    }

    /// MCP spec §3.2 step 4, the remote route.
    fn remote(&mut self, r: &Remote) -> Result<Server, Refusal> {
        let n = &self.opts.name;
        let transport = match &r.kind {
            TransportKind::StreamableHttp => Transport::Http,
            TransportKind::Sse => Transport::Sse,
            other => {
                return refuse(
                    format!(
                        "its remote is of type `{}`, which fl does not know",
                        printable(other.as_str())
                    ),
                    by_hand_url(n),
                );
            }
        };
        for (var, input) in names_in(&r.url, &r.variables) {
            if input.is_secret {
                return refuse(
                    format!(
                        "the remote URL names `{{{}}}`, which is a secret",
                        printable(var)
                    ),
                    by_hand_url(n),
                );
            }
        }
        let url = resolve(&r.url, &r.variables).or_else(|var| {
            refuse(
                format!(
                    "the remote URL names `{{{var}}}`, which has neither a value nor a default"
                ),
                by_hand_url(n),
            )
        })?;
        let mut headers = BTreeMap::new();
        for h in &r.headers {
            if let Some(value) = self.header(h)? {
                headers.insert(h.name.clone(), value);
            }
        }
        Ok(Server {
            from: None,
            version: None,
            enabled: true,
            vendors: None,
            transport,
            command: None,
            args: None,
            env: None,
            url: Some(url),
            headers: (!headers.is_empty()).then_some(headers),
        })
    }

    /// One header: a secret as a reference to `<SERVER>_<HEADER>`, or to
    /// `<SERVER>_<VAR>` for a value `{var}` or `<scheme> {var}`; a literal as
    /// it is; an optional header with no value left out.
    fn header(&mut self, h: &KeyValueInput) -> Result<Option<HeaderValue>, Refusal> {
        let n = &self.opts.name;
        let shown = printable(&h.name);
        if has_unseen(&h.name) {
            return unseen("its header name", n);
        }
        let next = format!("Add it by hand with `fl mcp add {n} --url <url> --header {shown}`");
        if !is_token(&h.name) {
            return refuse(
                format!(
                    "its header `{shown}` is not an HTTP header name, a shape fl cannot record"
                ),
                next,
            );
        }
        let template = h.value.as_deref().or(h.default.as_deref());
        let secret = h.is_secret
            || template.is_some_and(|t| names_in(t, &h.variables).any(|(_, i)| i.is_secret));
        if secret {
            let Some(template) = template else {
                let env = derived(n, &h.name);
                return Ok(Some(HeaderValue::Secret { env, scheme: None }));
            };
            let (scheme, rest) = match template.rsplit_once(' ') {
                Some((scheme, rest)) => (Some(scheme), rest),
                None => (None, template),
            };
            let var = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}'));
            if let Some(var) = var
                && h.variables.contains_key(var)
                && scheme.is_none_or(is_scheme)
            {
                return Ok(Some(HeaderValue::Secret {
                    env: derived(n, var),
                    scheme: scheme.map(str::to_string),
                }));
            }
            return refuse(
                format!(
                    "its secret header `{shown}` has the value `{}`, a shape fl cannot record",
                    printable(template)
                ),
                next,
            );
        }
        match template.map(|t| resolve(t, &h.variables)) {
            Some(Ok(value)) => Ok(Some(HeaderValue::Literal(value))),
            _ if h.is_required => refuse(
                format!("its header `{shown}` is required, and the registry gives it no value"),
                next,
            ),
            _ => Ok(None),
        }
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// An argument as a message shows it: a named one's flag and template, a
/// positional's hint or template.
fn label(a: &Argument) -> String {
    let template = a.value.as_deref().or(a.default.as_deref());
    let text = match (&a.kind, &a.name, template) {
        (ArgumentKind::Positional, ..) | (_, None, _) => (a.value_hint.as_deref().or(template))
            .unwrap_or("a positional argument")
            .to_string(),
        (_, Some(name), Some(t)) => format!("{name} {t}"),
        (_, Some(name), None) => name.clone(),
    };
    printable(&text)
}

/// docker's `-e NAME={var}` (or `--env`): the `NAME` it sets.
fn env_shape(a: &Argument) -> Option<String> {
    if a.kind != ArgumentKind::Named || !matches!(a.name.as_deref(), Some("-e" | "--env")) {
        return None;
    }
    let template = a.value.as_deref().or(a.default.as_deref())?;
    let (name, rest) = template.split_once('=')?;
    let var = rest.strip_prefix('{')?.strip_suffix('}')?;
    (is_env_name(name) && a.variables.contains_key(var)).then(|| name.to_string())
}

/// An OCI identifier pins exactly when its last path segment carries a tag
/// other than `latest`, or a digest (`@sha256:…`, whose colon reads as a tag
/// here, which is as good). A registry's port is in an earlier segment.
fn image_pin(id: &str) -> Result<(), String> {
    if id.starts_with('-') {
        return Err(format!(
            "its image `{}` starts with `-`, which docker would read as an option",
            printable(id)
        ));
    }
    let last = id.rsplit('/').next().unwrap_or(id);
    let id = printable(id);
    match last.split_once(':') {
        None => Err(format!(
            "its image `{id}` has neither a tag nor a digest, so the pin would not be exact"
        )),
        Some((_, "latest")) => Err(format!(
            "its image `{id}` is tagged `latest`, which moves, so the pin would not be exact"
        )),
        Some(_) => Ok(()),
    }
}

/// An exact npm version: `MAJOR.MINOR.PATCH`, numbers without leading zeros,
/// then an optional `-prerelease` and `+build`, each dot-separated identifiers
/// of letters, digits and `-`. A range, a tag or a wildcard is not one.
fn is_npm_version(version: &str) -> bool {
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let identifiers = |s: &str| {
        s.split('.')
            .all(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    };
    let number = |p: &str| {
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'))
    };
    let mut parts = core.split('.');
    let numbers = (0..3).all(|_| parts.next().is_some_and(number));
    numbers
        && parts.next().is_none()
        && pre.is_none_or(identifiers)
        && build.is_none_or(identifiers)
}

/// An exact PyPI version: it starts with a digit and holds letters, digits and
/// `.`, `!`, `+`, `-` only: no wildcard, operator or space.
fn is_pypi_version(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'!' | b'+' | b'-'))
}

/// An npm package name, `name` or `@scope/name`: lower-case letters, digits
/// and `.`, `_`, `~`, `-`, each part starting with neither `-` nor `.`. A URL,
/// a path or an option is not one.
fn is_npm_name(id: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['-', '.'])
            && p.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'_' | b'~' | b'-')
            })
    };
    match id.strip_prefix('@') {
        Some(rest) => rest
            .split_once('/')
            .is_some_and(|(scope, name)| part(scope) && part(name)),
        None => part(id),
    }
}

/// A PyPI project name: letters, digits and `.`, `_`, `-`, starting with a
/// letter or digit.
fn is_pypi_name(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// An HTTP field name (RFC 9110 `token`).
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// The refusal for a registry string that holds a control or invisible
/// character: it would be committed to the catalog or shown as another text.
fn unseen<T>(what: &str, n: &str) -> Result<T, Refusal> {
    refuse(
        format!("{what} holds a control or invisible character, which fl would have to commit"),
        format!(
            "Check the entry in the registry, or add the server by hand with \
             `fl mcp add {n} -- <command>` or `fl mcp add {n} --url <url>`"
        ),
    )
}

/// What a frozen entry holds that a person cannot see, named for a message.
fn unseen_field(server: &Server) -> Option<String> {
    if server.args.iter().flatten().any(|a| has_unseen(a)) {
        return Some("an argument it passes".to_string());
    }
    let env = server.env.iter().flatten();
    for (k, v) in env {
        if has_unseen(k) || matches!(v, EnvValue::Literal(x) if has_unseen(x)) {
            return Some(format!("its environment variable `{}`", printable(k)));
        }
    }
    if server.url.as_deref().is_some_and(has_unseen) {
        return Some("its remote URL".to_string());
    }
    for (k, v) in server.headers.iter().flatten() {
        if matches!(v, HeaderValue::Literal(x) if has_unseen(x)) {
            return Some(format!("its header `{}`", printable(k)));
        }
    }
    None
}

/// The warning for each literal value `server` records: it is committed with
/// the catalog (MCP spec §6). Names the field, never the value.
pub fn literal_warnings(server: &Server) -> Vec<String> {
    (server.literal_values().into_iter())
        .map(|field| {
            format!(
                "`{field}` is a literal value: it will be committed with the catalog, and is \
                 public if the repository is"
            )
        })
        .collect()
}

/// The warning for what a server added by hand records as given: the
/// arguments after `--` and the `--url`. A server's README often shows a key
/// there, and both are committed with the catalog (MCP spec §6). Says what is
/// recorded, never its text.
pub fn by_hand_warnings(server: &Server) -> Vec<String> {
    let mut warnings = Vec::new();
    if server.args.as_ref().is_some_and(|a| !a.is_empty()) {
        warnings.push(
            "the arguments after `--` are recorded as given: they are committed with the \
             catalog, and are public if the repository is. Pass a secret with `--env NAME`, \
             not as an argument"
                .to_string(),
        );
    }
    if server.url.is_some() {
        warnings.push(
            "the `--url` is recorded as given: it is committed with the catalog, and is \
             public if the repository is. Pass a secret with `--header NAME`, not in the URL"
                .to_string(),
        );
    }
    warnings
}

/// An HTTP authentication scheme, such as `Bearer`: one token of letters,
/// digits and `-`. `fl mcp add --header` takes a header name in the same
/// shape.
pub fn is_scheme(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `<SERVER>_<PART>`, uppercased, `-` as `_`: the variable a secret header
/// reads when the registry or the person names none.
pub fn derived(server: &str, part: &str) -> String {
    format!("{server}_{part}")
        .to_ascii_uppercase()
        .replace('-', "_")
}

/// Each `{name}` in `template` that `variables` defines, in order. A `{name}`
/// it does not define is text, as the registry's schema says.
fn names_in<'a>(
    template: &'a str,
    variables: &'a BTreeMap<String, Input>,
) -> impl Iterator<Item = (&'a str, &'a Input)> {
    template.split('{').skip(1).filter_map(move |part| {
        let name = part.split_once('}')?.0;
        variables.get_key_value(name).map(|(k, i)| (k.as_str(), i))
    })
}

/// `template` with each variable it names replaced by the variable's value,
/// else its default. `Err`: the first variable with neither.
fn resolve(template: &str, variables: &BTreeMap<String, Input>) -> Result<String, String> {
    let mut out = template.to_string();
    for (name, input) in names_in(template, variables) {
        let Some(value) = input.value.as_deref().or(input.default.as_deref()) else {
            return Err(printable(name));
        };
        out = out.replace(&format!("{{{name}}}"), value);
    }
    Ok(out)
}

/// One field of a launch spec that `upgrade` would change (MCP spec §3.3).
/// `old` and `new` are shown as in the catalog; `None` is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub field: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "(none)".to_string());
        write!(
            f,
            "{}: {} -> {}",
            self.field,
            show(&self.old),
            show(&self.new)
        )
    }
}

/// Every field of the launch spec that differs between `old` and `new`, in
/// the catalog's order: `from`, `version`, `transport`, `command`, `args`,
/// each `env.<NAME>`, `url`, each `headers.<NAME>`. A person's own choices,
/// `enabled` and `vendors`, are not part of it.
pub fn diff(old: &Server, new: &Server) -> Vec<Change> {
    let mut out = Vec::new();
    let mut field = |name: String, a: Option<String>, b: Option<String>| {
        if a != b {
            out.push(Change {
                field: name,
                old: a,
                new: b,
            });
        }
    };
    let text = |s: &Option<String>| s.as_ref().map(|s| format!("{s:?}"));
    field("from".into(), text(&old.from), text(&new.from));
    field("version".into(), text(&old.version), text(&new.version));
    let transport = |s: &Server| Some(format!("{:?}", s.transport.as_str()));
    field("transport".into(), transport(old), transport(new));
    field("command".into(), text(&old.command), text(&new.command));
    let args = |s: &Server| s.args.as_ref().map(|a| format!("{a:?}"));
    field("args".into(), args(old), args(new));
    let (a, b) = (
        old.env.clone().unwrap_or_default(),
        new.env.clone().unwrap_or_default(),
    );
    for k in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        field(
            format!("env.{k}"),
            a.get(k).map(show_env),
            b.get(k).map(show_env),
        );
    }
    field("url".into(), text(&old.url), text(&new.url));
    let (a, b) = (
        old.headers.clone().unwrap_or_default(),
        new.headers.clone().unwrap_or_default(),
    );
    for k in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        field(
            format!("headers.{k}"),
            a.get(k).map(show_header),
            b.get(k).map(show_header),
        );
    }
    out
}

fn show_env(v: &EnvValue) -> String {
    match v {
        EnvValue::Literal(s) => format!("{s:?}"),
        EnvValue::Secret { env: None } => "{ secret = true }".to_string(),
        EnvValue::Secret { env: Some(e) } => format!("{{ secret = true, env = {e:?} }}"),
    }
}

fn show_header(v: &HeaderValue) -> String {
    match v {
        HeaderValue::Literal(s) => format!("{s:?}"),
        HeaderValue::Secret { env, scheme: None } => format!("{{ secret = true, env = {env:?} }}"),
        HeaderValue::Secret {
            env,
            scheme: Some(s),
        } => format!("{{ secret = true, env = {env:?}, scheme = {s:?} }}"),
    }
}

/// `upgrade`'s rule (MCP spec §3.3): the registry's version must be newer
/// than the pinned one by semantic-version precedence, unless the person
/// named it with `--to` (`named`). A version that is not a semantic version
/// cannot be compared, and is refused unless named.
pub fn check_upgrade(name: &str, pinned: &str, offered: &str, named: bool) -> Result<(), McpError> {
    if named {
        return Ok(());
    }
    let (pinned, offered) = (printable(pinned), printable(offered));
    let problem = match (SemVer::parse(&pinned), SemVer::parse(&offered)) {
        (Some(p), Some(o)) if o.precedence(&p).is_gt() => return Ok(()),
        (Some(_), Some(_)) => {
            format!("it is pinned to {pinned}, and the registry's {offered} is not newer")
        }
        _ => format!(
            "it is pinned to {pinned}, and the registry's {offered} cannot be compared with it \
             as a semantic version"
        ),
    };
    Err(McpError::NotNewer {
        name: name.to_string(),
        problem,
        next: format!("`fl mcp upgrade {name} --to {offered}` moves it there anyway"),
    })
}

/// A semantic version, for its precedence: the core, then the pre-release
/// identifiers; build metadata does not count.
struct SemVer<'a> {
    core: [u64; 3],
    pre: Vec<&'a str>,
}

impl<'a> SemVer<'a> {
    fn parse(v: &'a str) -> Option<SemVer<'a>> {
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre): (&str, Vec<&str>) = match v.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').collect()),
            None => (v, Vec::new()),
        };
        if pre.iter().any(|p| p.is_empty()) {
            return None;
        }
        let mut parts = core.split('.').map(number);
        let core = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some(SemVer { core, pre })
    }

    fn precedence(&self, other: &SemVer) -> Ordering {
        let pre = match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let pairs = self.pre.iter().zip(&other.pre);
                let first = pairs.map(|(a, b)| identifier(a, b)).find(|o| o.is_ne());
                first.unwrap_or_else(|| self.pre.len().cmp(&other.pre.len()))
            }
        };
        self.core.cmp(&other.core).then(pre)
    }
}

/// A numeric part. Rust's parse also takes a leading `+`, but none reaches
/// here: a `+` starts the build metadata, which is cut off first.
fn number(s: &str) -> Option<u64> {
    s.parse().ok()
}

/// Pre-release identifiers: numbers by value and below words, words by
/// their ASCII order.
fn identifier(a: &str, b: &str) -> Ordering {
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, Editor};
    use crate::fake::{self, FakeRegistry};
    use crate::registry::Registry;
    use serde_json::{Value, json};

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn named(name: &str) -> FreezeOptions {
        FreezeOptions {
            name: name.to_string(),
            ..FreezeOptions::default()
        }
    }

    fn routed(name: &str, route: Route) -> FreezeOptions {
        FreezeOptions {
            route: Some(route),
            ..named(name)
        }
    }

    /// One fixture of the fake registry, as `add --from` reads it.
    fn fixture(name: &str, version: &str) -> ServerResponse {
        let fake = FakeRegistry::start();
        Registry::new(&fake.url())
            .unwrap()
            .version(name, version)
            .unwrap()
    }

    fn response(server: Value) -> ServerResponse {
        response_with(server, "active", None)
    }

    fn response_with(server: Value, status: &str, message: Option<&str>) -> ServerResponse {
        let mut official = json!({
            "status": status,
            "statusChangedAt": "2026-10-01T00:00:00Z",
            "publishedAt": "2026-10-01T00:00:00Z",
            "isLatest": true
        });
        if let Some(m) = message {
            official["statusMessage"] = json!(m);
        }
        serde_json::from_value(json!({
            "server": server,
            "_meta": { "io.modelcontextprotocol.registry/official": official }
        }))
        .unwrap()
    }

    /// `io.example/sample` 1.0.0 with these packages and remotes.
    fn sample(packages: Value, remotes: Value) -> Value {
        json!({
            "name": "io.example/sample",
            "description": "A sample server.",
            "version": "1.0.0",
            "packages": packages,
            "remotes": remotes
        })
    }

    fn with(mut base: Value, extra: Value) -> Value {
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    fn npm(extra: Value) -> Value {
        let base = json!({
            "registryType": "npm",
            "identifier": "@example/sample-mcp",
            "version": "1.0.0",
            "transport": { "type": "stdio" }
        });
        with(base, extra)
    }

    fn oci(extra: Value) -> Value {
        let base = json!({
            "registryType": "oci",
            "identifier": "ghcr.io/example/sample-mcp:1.0.0",
            "transport": { "type": "stdio" }
        });
        with(base, extra)
    }

    fn remote(extra: Value) -> Value {
        let base = json!({ "type": "streamable-http", "url": "https://sample.example.com/mcp" });
        with(base, extra)
    }

    /// The real shape of GitHub's server 2.0.1 (an OCI package whose token
    /// is an optional `-e NAME={token}` runtime argument, and a remote),
    /// under a fixture name.
    fn github() -> ServerResponse {
        response(json!({
            "name": "io.example/github",
            "description": "Connect AI assistants to GitHub.",
            "version": "2.0.1",
            "packages": [{
                "registryType": "oci",
                "identifier": "ghcr.io/example/github-mcp-server:2.0.1",
                "transport": { "type": "stdio" },
                "runtimeArguments": [
                    { "value": "127.0.0.1:8085:8085", "type": "named", "name": "-p" },
                    { "value": "GITHUB_OAUTH_CALLBACK_PORT=8085", "type": "named", "name": "-e" },
                    { "description": "Optional GitHub Personal Access Token.",
                      "value": "GITHUB_PERSONAL_ACCESS_TOKEN={token}",
                      "variables": { "token": { "format": "string", "isSecret": true } },
                      "type": "named", "name": "-e" }
                ]
            }],
            "remotes": [{
                "type": "streamable-http", "url": "https://api.example.com/mcp/",
                "headers": [{ "isSecret": true, "name": "Authorization" }]
            }]
        }))
    }

    /// The catalog takes the frozen entry as it is, and reads it back
    /// equal; the catalog's text, for a look at what is committed.
    fn committed(name: &str, server: &Server) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut editor = Editor::open(dir.path()).unwrap();
        editor.add(name, server).unwrap();
        let text = editor.text();
        let back = Catalog::parse(&text, editor.path()).unwrap();
        assert_eq!(&back.servers[name], server, "{text}");
        text
    }

    fn secret() -> EnvValue {
        EnvValue::Secret { env: None }
    }

    fn literal(value: &str) -> EnvValue {
        EnvValue::Literal(value.to_string())
    }

    // The rules a hand-made entry shares with a frozen one live here once.
    #[test]
    fn literal_warnings_name_the_field_and_never_the_value() {
        let server = Server {
            from: None,
            version: None,
            enabled: true,
            vendors: None,
            transport: Transport::Stdio,
            command: Some("node".into()),
            args: None,
            env: Some(BTreeMap::from([
                ("LOG".to_string(), literal("hunter2")),
                ("TOKEN".to_string(), secret()),
            ])),
            url: None,
            headers: None,
        };
        assert_eq!(
            literal_warnings(&server),
            [
                "`env.LOG` is a literal value: it will be committed with the catalog, and is \
              public if the repository is"
            ]
        );
    }

    #[test]
    fn derived_names_and_one_token_shapes_are_shared_rules() {
        assert_eq!(derived("my-docs", "X-Team"), "MY_DOCS_X_TEAM");
        assert!(is_scheme("Bearer") && is_scheme("X-Team"));
        assert!(!is_scheme("") && !is_scheme("a b") && !is_scheme("a_b"));
    }

    // MCP spec §3.2 step 4: npm runs as `npx -y <identifier>@<version>`,
    // with the package arguments that have a value appended.
    #[test]
    fn an_npm_package_freezes_to_npx_at_its_exact_version() {
        let frozen = freeze(&fixture(fake::NOTES, "latest"), &named("notes")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.from.as_deref(), Some(fake::NOTES));
        assert_eq!(s.version.as_deref(), Some("1.2.0"));
        assert!(s.enabled);
        assert_eq!(s.vendors, None);
        assert_eq!(s.transport, Transport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        let args = strings(&["-y", "@example/notes-mcp@1.2.0", "./notes"]);
        assert_eq!(s.args, Some(args));
        let env = BTreeMap::from([
            ("NOTES_LOG".to_string(), literal("info")),
            ("NOTES_TOKEN".to_string(), secret()),
        ]);
        assert_eq!(s.env, Some(env));
        assert_eq!((&s.url, &s.headers), (&None, &None));
        assert_eq!(frozen.secrets, ["NOTES_TOKEN"]);
        assert_eq!(frozen.warnings.len(), 1, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`env.NOTES_LOG` is a literal value"));
        assert!(frozen.warnings[0].contains("will be committed with the catalog"));
        assert!(frozen.notes.is_empty());
        committed("notes", s);
    }

    #[test]
    fn a_pypi_package_freezes_to_uvx_at_its_exact_version() {
        let frozen = freeze(&fixture(fake::WEATHER, "latest"), &named("weather")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.version.as_deref(), Some("0.4.1"));
        assert_eq!(s.command.as_deref(), Some("uvx"));
        let args = strings(&["example-weather-mcp==0.4.1", "--units", "metric"]);
        assert_eq!(s.args, Some(args));
        let env = BTreeMap::from([("WEATHER_API_KEY".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["WEATHER_API_KEY"]);
        assert!(frozen.warnings.is_empty(), "{:?}", frozen.warnings);
        committed("weather", s);
    }

    // MCP spec §3.2 step 4: `docker run -i --rm`, the runtime arguments, one
    // `-e NAME` per variable, and the identifier as given.
    #[test]
    fn an_oci_package_freezes_to_docker_run_with_the_image_as_given() {
        // The token's argument is optional (its variable is required once
        // the argument is passed), so it is named to be included.
        let mut opts = named("tracker");
        opts.with.insert("TRACKER_TOKEN".to_string());
        let frozen = freeze(&fixture(fake::TRACKER, "latest"), &opts).unwrap();
        let s = &frozen.server;
        assert_eq!(s.version.as_deref(), Some("2.0.1"));
        assert_eq!(s.command.as_deref(), Some("docker"));
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "TRACKER_PORT=8085",
            "-e",
            "TRACKER_TOKEN",
            fake::TRACKER_IMAGE,
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("TRACKER_TOKEN".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["TRACKER_TOKEN"]);
        committed("tracker", s);

        // Each environment variable is passed into the container by name; a
        // digest pins as well as a tag; a registry's port is not a tag.
        for image in [
            "ghcr.io/example/sample-mcp@sha256:0123456789abcdef",
            "registry.example.com:5000/example/sample-mcp:1.0.0",
        ] {
            let package = oci(json!({
                "identifier": image,
                "environmentVariables": [
                    { "name": "SAMPLE_REGION", "default": "eu" },
                    { "name": "SAMPLE_KEY", "isSecret": true, "isRequired": true },
                    { "name": "SAMPLE_DEBUG" }
                ]
            }));
            let frozen = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
            let s = frozen.unwrap().server;
            let args = [
                "run",
                "-i",
                "--rm",
                "-e",
                "SAMPLE_REGION",
                "-e",
                "SAMPLE_KEY",
                image,
            ];
            assert_eq!(s.args, Some(strings(&args)));
            let env = BTreeMap::from([
                ("SAMPLE_KEY".to_string(), secret()),
                ("SAMPLE_REGION".to_string(), literal("eu")),
            ]);
            assert_eq!(s.env, Some(env));
        }

        // A secret in an argument is taken only as docker's own
        // `-e NAME={var}`, with `NAME` a variable name and `{var}` defined.
        let token = json!({ "token": { "isSecret": true } });
        let refused = [
            (
                "runtimeArguments",
                "--label",
                "SAMPLE_TOKEN={token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "sample-token={token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "SAMPLE_TOKEN=x{token}",
                token.clone(),
                false,
            ),
            (
                "runtimeArguments",
                "-e",
                "SAMPLE_TOKEN={nope}",
                json!({}),
                true,
            ),
            (
                "packageArguments",
                "-e",
                "SAMPLE_TOKEN={token}",
                token,
                false,
            ),
        ];
        for (list, name, value, variables, secret) in refused {
            let argument = json!({
                "type": "named", "name": name, "value": value, "isRequired": true,
                "isSecret": secret, "variables": variables
            });
            let package = oci(json!({ (list): [argument] }));
            let err = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
            let msg = err.unwrap_err().to_string();
            assert!(
                msg.contains("holds a secret"),
                "{list} {name} {value}: {msg}"
            );
        }
    }

    // MCP spec §3.2 step 4: an optional argument that carries a secret is left
    // out, with a note, unless `--with <NAME>` names it; then the docker
    // `-e NAME={var}` becomes `-e NAME` and a secret reference.
    #[test]
    fn an_optional_secret_argument_is_left_out_unless_named_with_with() {
        let frozen = freeze(&github(), &routed("github", Route::Oci)).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-p",
            "127.0.0.1:8085:8085",
            "-e",
            "GITHUB_OAUTH_CALLBACK_PORT=8085",
            "ghcr.io/example/github-mcp-server:2.0.1",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        assert_eq!(s.env, None);
        assert!(frozen.secrets.is_empty());
        assert_eq!(frozen.notes.len(), 1, "{:?}", frozen.notes);
        assert!(frozen.notes[0].contains("it is optional and needs a secret"));
        assert!(frozen.notes[0].contains("`--with GITHUB_PERSONAL_ACCESS_TOKEN` includes it"));
        committed("github", s);

        let mut opts = routed("github", Route::Oci);
        opts.with.insert("GITHUB_PERSONAL_ACCESS_TOKEN".to_string());
        let frozen = freeze(&github(), &opts).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-p",
            "127.0.0.1:8085:8085",
            "-e",
            "GITHUB_OAUTH_CALLBACK_PORT=8085",
            "-e",
            "GITHUB_PERSONAL_ACCESS_TOKEN",
            "ghcr.io/example/github-mcp-server:2.0.1",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["GITHUB_PERSONAL_ACCESS_TOKEN"]);
        assert!(frozen.notes.is_empty(), "{:?}", frozen.notes);
        committed("github", s);
    }

    // As for an optional argument: an optional secret variable is left out,
    // with a note, unless `--with <NAME>` names it, or the pinned entry had it.
    #[test]
    fn an_optional_secret_variable_is_left_out_unless_named_with_with() {
        let package = oci(json!({
            "environmentVariables": [
                { "name": "SAMPLE_KEY", "isSecret": true, "isRequired": true },
                { "name": "SAMPLE_EXTRA", "isSecret": true }
            ]
        }));
        let entry = response(sample(json!([package]), json!([])));
        let frozen = freeze(&entry, &named("sample")).unwrap();
        let s = &frozen.server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_KEY",
            "ghcr.io/example/sample-mcp:1.0.0",
        ];
        assert_eq!(s.args, Some(strings(&args)));
        let env = BTreeMap::from([("SAMPLE_KEY".to_string(), secret())]);
        assert_eq!(s.env, Some(env));
        assert_eq!(frozen.secrets, ["SAMPLE_KEY"]);
        assert_eq!(frozen.notes.len(), 1, "{:?}", frozen.notes);
        assert!(
            frozen.notes[0].contains("Left out `SAMPLE_EXTRA`: it is optional and needs a secret")
        );
        assert!(frozen.notes[0].contains("`--with SAMPLE_EXTRA` includes it"));

        let mut opts = named("sample");
        opts.with.insert("SAMPLE_EXTRA".to_string());
        let frozen = freeze(&entry, &opts).unwrap();
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_KEY",
            "-e",
            "SAMPLE_EXTRA",
            "ghcr.io/example/sample-mcp:1.0.0",
        ];
        assert_eq!(frozen.server.args, Some(strings(&args)));
        let env = BTreeMap::from([
            ("SAMPLE_EXTRA".to_string(), secret()),
            ("SAMPLE_KEY".to_string(), secret()),
        ]);
        assert_eq!(frozen.server.env, Some(env.clone()));
        assert_eq!(frozen.secrets, ["SAMPLE_EXTRA", "SAMPLE_KEY"]);
        assert!(frozen.notes.is_empty(), "{:?}", frozen.notes);
        committed("sample", &frozen.server);

        // `upgrade` keeps it.
        let carried = FreezeOptions::upgrading("sample", &frozen.server);
        assert!(carried.kept_with.contains("SAMPLE_EXTRA"));
        let next = freeze(&entry, &carried).unwrap().server;
        assert_eq!(next.env, Some(env));
    }

    // MCP spec §3.2 step 4: a secret header with no value reads the variable
    // `<SERVER>_<HEADER>`, which holds the whole value; `Bearer {token}`
    // keeps its scheme.
    #[test]
    fn a_remote_freezes_to_its_url_and_a_secret_header_to_a_derived_variable() {
        let frozen = freeze(&fixture(fake::DOCS, "latest"), &named("docs")).unwrap();
        let s = &frozen.server;
        assert_eq!(s.from.as_deref(), Some(fake::DOCS));
        assert_eq!(s.version.as_deref(), Some("1.0.0"));
        assert_eq!(s.transport, Transport::Http);
        assert_eq!(s.url.as_deref(), Some(fake::DOCS_URL));
        assert_eq!((&s.command, &s.args, &s.env), (&None, &None, &None));
        let auth = HeaderValue::Secret {
            env: "DOCS_AUTHORIZATION".to_string(),
            scheme: None,
        };
        let headers = BTreeMap::from([("Authorization".to_string(), auth)]);
        assert_eq!(s.headers, Some(headers));
        assert_eq!(frozen.secrets, ["DOCS_AUTHORIZATION"]);
        committed("docs", s);

        // The catalog name, not the registry's, names the variable.
        let frozen = freeze(&fixture(fake::DOCS, "latest"), &named("team-docs")).unwrap();
        assert_eq!(frozen.secrets, ["TEAM_DOCS_AUTHORIZATION"]);
        // A catalog name may start with a digit; a variable may not.
        let err = freeze(&fixture(fake::DOCS, "latest"), &named("9docs")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("`9DOCS_AUTHORIZATION` cannot be an environment variable name"),
            "{msg}"
        );

        let sse = json!({
            "type": "sse",
            "url": "https://{tenant}.example.com/sse",
            "variables": { "tenant": { "default": "acme" } },
            "headers": [
                { "name": "Authorization", "value": "Bearer {token}",
                  "variables": { "token": { "isSecret": true } } },
                { "name": "X-Api-Key", "value": "{key}",
                  "variables": { "key": { "isSecret": true } } },
                { "name": "X-Region", "value": "eu", "default": "us" },
                { "name": "X-Team", "value": "{team}",
                  "variables": { "team": { "default": "widgets" } } },
                { "name": "X-Trace" }
            ]
        });
        let frozen = freeze(&response(sample(json!([]), json!([sse]))), &named("sample"));
        let frozen = frozen.unwrap();
        let s = &frozen.server;
        assert_eq!(s.transport, Transport::Sse);
        assert_eq!(s.url.as_deref(), Some("https://acme.example.com/sse"));
        let headers = BTreeMap::from([
            (
                "Authorization".to_string(),
                HeaderValue::Secret {
                    env: "SAMPLE_TOKEN".to_string(),
                    scheme: Some("Bearer".to_string()),
                },
            ),
            (
                "X-Api-Key".to_string(),
                HeaderValue::Secret {
                    env: "SAMPLE_KEY".to_string(),
                    scheme: None,
                },
            ),
            (
                "X-Region".to_string(),
                HeaderValue::Literal("eu".to_string()),
            ),
            (
                "X-Team".to_string(),
                HeaderValue::Literal("widgets".to_string()),
            ),
        ]);
        assert_eq!(s.headers, Some(headers));
        assert_eq!(frozen.secrets, ["SAMPLE_KEY", "SAMPLE_TOKEN"]);
        assert_eq!(frozen.warnings.len(), 2, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`headers.X-Region` is a literal value"));
        assert!(frozen.warnings[1].contains("`headers.X-Team` is a literal value"));
        committed("sample", s);

        // A scheme is one word, and the variable must be defined.
        for (value, variables) in [
            ("Bearer x {token}", json!({ "token": {} })),
            (" {token}", json!({ "token": {} })),
            ("Bearer {nope}", json!({})),
        ] {
            let header = json!({
                "name": "Authorization", "value": value, "isSecret": true,
                "variables": variables
            });
            let r = remote(json!({ "headers": [header] }));
            let err = freeze(&response(sample(json!([]), json!([r]))), &named("sample"));
            let msg = err.unwrap_err().to_string();
            assert!(msg.contains("a shape fl cannot record"), "{value}: {msg}");
        }
    }

    // MCP spec §3.2 step 4: how each argument is passed.
    #[test]
    fn arguments_render_by_kind_format_and_variables() {
        let package = npm(json!({
            "runtimeArguments": [
                { "type": "named", "name": "--prefer-offline", "value": "true",
                  "format": "boolean" },
                { "type": "positional", "value": "--no-update-notifier" }
            ],
            "packageArguments": [
                { "type": "named", "name": "--port", "value": "8080" },
                { "type": "named", "name": "--level", "value": "debug", "default": "info" },
                { "type": "named", "name": "--verbose", "value": "true", "format": "boolean" },
                { "type": "named", "name": "--quiet", "default": "false", "format": "boolean" },
                { "type": "positional", "valueHint": "dir", "value": "{root}/data",
                  "variables": { "root": { "default": "/srv" } } },
                { "type": "named", "name": "--mode", "value": "{mode}",
                  "variables": { "mode": { "value": "fast", "default": "slow" } } },
                { "type": "named", "name": "--color" },
                { "type": "named", "name": "--region", "value": "{region}",
                  "variables": { "region": {} } },
                { "type": "positional", "value": "{literal}" }
            ]
        }));
        let frozen = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
        let args = [
            "-y",
            "--prefer-offline",
            "--no-update-notifier",
            "@example/sample-mcp@1.0.0",
            "--port",
            "8080",
            "--level",
            "debug",
            "--verbose",
            "/srv/data",
            "--mode",
            "fast",
            "{literal}",
        ];
        assert_eq!(frozen.unwrap().server.args, Some(strings(&args)));

        // A docker `-e NAME={var}` is only docker's: for npx it is an
        // argument that holds a secret.
        let package = npm(json!({
            "runtimeArguments": [
                { "type": "named", "name": "-e", "value": "SAMPLE_TOKEN={token}",
                  "isRequired": true, "variables": { "token": { "isSecret": true } } }
            ]
        }));
        let err = freeze(&response(sample(json!([package]), json!([]))), &named("s"));
        assert!(err.unwrap_err().to_string().contains("holds a secret"));
    }

    // MCP spec §3.2 step 5: a required variable with no value takes one from
    // `--env`, recorded as a literal and warned about.
    #[test]
    fn a_required_variable_takes_its_value_from_env_and_is_warned_as_committed() {
        let package = npm(json!({
            "environmentVariables": [
                { "name": "SAMPLE_HOME", "isRequired": true },
                { "name": "SAMPLE_DEBUG" },
                { "name": "SAMPLE_LEVEL", "default": "info" },
                { "name": "SAMPLE_MODE", "value": "fixed", "default": "other" },
                { "name": "SAMPLE_URL", "value": "https://{host}/",
                  "variables": { "host": { "default": "example.com" } } },
                { "name": "SAMPLE_AUTH", "value": "Bearer {token}", "isRequired": true,
                  "variables": { "token": { "isSecret": true } } }
            ]
        }));
        let entry = response(sample(json!([package]), json!([])));
        let mut opts = named("sample");
        opts.env
            .insert("SAMPLE_HOME".to_string(), "/srv/sample".to_string());
        opts.env
            .insert("SAMPLE_LEVEL".to_string(), "debug".to_string());
        let frozen = freeze(&entry, &opts).unwrap();
        // A variable whose value names a secret is a secret: the person
        // sets its whole value.
        let env = BTreeMap::from([
            ("SAMPLE_AUTH".to_string(), secret()),
            ("SAMPLE_HOME".to_string(), literal("/srv/sample")),
            ("SAMPLE_LEVEL".to_string(), literal("debug")),
            ("SAMPLE_MODE".to_string(), literal("fixed")),
            ("SAMPLE_URL".to_string(), literal("https://example.com/")),
        ]);
        assert_eq!(frozen.server.env, Some(env));
        assert_eq!(frozen.secrets, ["SAMPLE_AUTH"]);
        assert_eq!(frozen.warnings.len(), 4, "{:?}", frozen.warnings);
        assert!(frozen.warnings[0].contains("`env.SAMPLE_HOME` is a literal value"));
        assert!(frozen.warnings[1].contains("`env.SAMPLE_LEVEL` is a literal value"));
    }

    // MCP spec §3.2 step 3.
    #[test]
    fn a_route_is_chosen_with_package_or_remote() {
        let multi = fixture(fake::MULTI, "latest");
        let cases = [
            (Route::Npm, "npx", vec!["-y", "@example/multi-mcp@3.0.0"]),
            (
                Route::Oci,
                "docker",
                vec!["run", "-i", "--rm", "ghcr.io/example/multi-mcp:3.0.0"],
            ),
        ];
        for (route, command, args) in cases {
            let s = freeze(&multi, &routed("multi", route)).unwrap().server;
            assert_eq!(s.command.as_deref(), Some(command));
            assert_eq!(s.args, Some(strings(&args)));
            assert_eq!(Route::of(&s), Some(route));
        }
        let s = freeze(&multi, &routed("multi", Route::Remote))
            .unwrap()
            .server;
        assert_eq!(s.transport, Transport::Sse);
        assert_eq!(s.url.as_deref(), Some("https://multi.example.com/sse"));
        assert_eq!(s.headers, None);
        assert_eq!(Route::of(&s), Some(Route::Remote));
        let pypi = freeze(&fixture(fake::WEATHER, "latest"), &named("w")).unwrap();
        assert_eq!(Route::of(&pypi.server), Some(Route::Pypi));
        let by_hand = Server {
            command: Some("node".to_string()),
            ..pypi.server
        };
        assert_eq!(Route::of(&by_hand), None);
    }

    // MCP spec §3.2 step 2.
    #[test]
    fn a_deleted_server_is_refused_and_a_deprecated_one_warned_about() {
        let err = freeze(&fixture(fake::GONE, "latest"), &named("gone")).unwrap_err();
        assert!(matches!(err, McpError::Unfreezable { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("`io.example/gone` 1.0.0 cannot be frozen"),
            "{msg}"
        );
        assert!(msg.contains(&format!(
            "is deleted in the registry ({})",
            fake::GONE_MESSAGE
        )));

        let frozen = freeze(&fixture(fake::LEGACY, "latest"), &named("legacy")).unwrap();
        assert_eq!(frozen.server.command.as_deref(), Some("uvx"));
        assert_eq!(frozen.warnings.len(), 1, "{:?}", frozen.warnings);
        let warning = &frozen.warnings[0];
        assert!(
            warning.contains("is deprecated in the registry"),
            "{warning}"
        );
        assert!(warning.contains(fake::LEGACY_MESSAGE), "{warning}");
    }

    // MCP spec §3.2 step 1: the version the registry returned, never the
    // word `latest`.
    #[test]
    fn the_frozen_entry_records_the_registrys_version_never_latest() {
        for (name, version) in [
            (fake::NOTES, "1.2.0"),
            (fake::TRACKER, "2.0.1"),
            (fake::DOCS, "1.0.0"),
        ] {
            let frozen = freeze(&fixture(name, "latest"), &named("s")).unwrap();
            assert_eq!(frozen.server.version.as_deref(), Some(version));
            let text = committed("s", &frozen.server);
            assert!(!text.contains("latest"), "{text}");
        }
        let frozen = freeze(&fixture(fake::NOTES, "1.0.0"), &named("s")).unwrap();
        assert_eq!(frozen.server.version.as_deref(), Some("1.0.0"));
        let args = strings(&["-y", "@example/notes-mcp@1.0.0", "./notes"]);
        assert_eq!(frozen.server.args, Some(args));
    }

    // MCP spec §3.2: each entry fl cannot freeze honestly is refused before
    // anything is written, with its own phrase and the way on.
    #[test]
    fn each_unfreezable_entry_is_refused_naming_the_way_on() {
        const VALUE: &str = "s3cr3t-value-0123456789";
        let plain = |packages: Value| response(sample(packages, json!([])));
        let remote_only = |r: Value| response(sample(json!([]), json!([r])));
        let opts_with = |f: &dyn Fn(&mut FreezeOptions)| {
            let mut o = named("sample");
            f(&mut o);
            o
        };
        let header = |h: Value| remote_only(remote(json!({ "headers": [h] })));
        let cases: Vec<(&str, ServerResponse, FreezeOptions, &str, &str)> = vec![
            (
                "deleted",
                response_with(
                    sample(json!([npm(json!({}))]), json!([])),
                    "deleted",
                    Some("Withdrawn."),
                ),
                named("sample"),
                "is deleted in the registry (Withdrawn.)",
                "`fl mcp search <text>` lists",
            ),
            (
                "no route",
                plain(json!([])),
                named("sample"),
                "offers no launch route",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "several routes",
                response(sample(
                    json!([
                        npm(json!({})),
                        oci(json!({})),
                        npm(json!({ "registryType": "mcpb" }))
                    ]),
                    json!([remote(json!({ "type": "sse" }))]),
                )),
                named("sample"),
                "offers more than one launch route: `--package npm` (@example/sample-mcp), \
                 `--package oci` (ghcr.io/example/sample-mcp:1.0.0), a `mcpb` package, which \
                 fl cannot freeze, `--remote` (sse, https://sample.example.com/mcp)",
                "Choose one with `--package <type>` or `--remote`",
            ),
            (
                "a route not on offer",
                plain(json!([npm(json!({}))])),
                routed("sample", Route::Pypi),
                "offers no `--package pypi` route, only `--package npm`",
                "Choose one it offers",
            ),
            (
                "a route offered twice",
                response(sample(
                    json!([]),
                    json!([remote(json!({})), remote(json!({ "type": "sse" }))]),
                )),
                routed("sample", Route::Remote),
                "offers the `--remote` route more than once",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "the server's version is not exact",
                response(with(
                    sample(json!([npm(json!({}))]), json!([])),
                    json!({ "version": "latest" }),
                )),
                named("sample"),
                "its version `latest` is not an exact version",
                "add the server by hand",
            ),
            (
                "a package with no version",
                plain(json!([npm(json!({ "version": null }))])),
                named("sample"),
                "names no exact version",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "mcpb",
                plain(json!([npm(json!({ "registryType": "mcpb" }))])),
                named("sample"),
                "a `mcpb` package; fl freezes npm, PyPI and OCI packages only",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "nuget",
                plain(json!([npm(json!({ "registryType": "nuget" }))])),
                named("sample"),
                "a `nuget` package",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "an unknown registry type",
                plain(json!([npm(json!({ "registryType": "gem" }))])),
                named("sample"),
                "a `gem` package",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a package that serves HTTP",
                plain(json!([npm(json!({
                    "transport": { "type": "streamable-http",
                                   "url": "http://localhost:{port}/mcp" }
                }))])),
                named("sample"),
                "its package's transport is `streamable-http`, not stdio",
                "Start it yourself",
            ),
            (
                "an untagged image",
                plain(json!([oci(json!({
                    "identifier": "registry.example.com:5000/example/sample-mcp"
                }))])),
                named("sample"),
                "has neither a tag nor a digest",
                "naming an exact image",
            ),
            (
                "an image tagged latest",
                plain(json!([oci(json!({
                    "identifier": "ghcr.io/example/sample-mcp:latest"
                }))])),
                named("sample"),
                "is tagged `latest`",
                "naming an exact image",
            ),
            (
                "a positional runtime argument",
                plain(json!([oci(json!({
                    "runtimeArguments": [
                        { "type": "positional", "value": "run" },
                        { "type": "named", "name": "--rm", "value": "true",
                          "format": "boolean" }
                    ]
                }))])),
                named("sample"),
                "a positional runtime argument (`run`)",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a secret in an argument that is not a docker -e",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--token", "value": "{token}",
                          "isRequired": true,
                          "variables": { "token": { "isSecret": true } } }
                    ]
                }))])),
                named("sample"),
                "its argument `--token {token}` holds a secret",
                "passing the secret in an environment variable",
            ),
            (
                "an argument that is itself a secret",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--api-key", "isSecret": true,
                          "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `--api-key` holds a secret",
                "passing the secret in an environment variable",
            ),
            (
                "a required argument with no value",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "positional", "valueHint": "notes_dir", "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `notes_dir` is required and has no value",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a required argument naming an unset variable",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "named", "name": "--root", "value": "{root}",
                          "isRequired": true, "variables": { "root": {} } }
                    ]
                }))])),
                named("sample"),
                "names an unset variable, `{root}`",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a remote URL variable with no default",
                remote_only(remote(json!({
                    "url": "https://{tenant}.example.com/mcp",
                    "variables": { "tenant": { "isRequired": true } }
                }))),
                named("sample"),
                "the remote URL names `{tenant}`, which has neither a value nor a default",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a secret remote URL variable",
                remote_only(remote(json!({
                    "url": "https://sample.example.com/{key}/mcp",
                    "variables": { "key": { "isSecret": true, "default": "k" } }
                }))),
                named("sample"),
                "the remote URL names `{key}`, which is a secret",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a remote of an unknown type",
                remote_only(remote(json!({ "type": "websocket" }))),
                named("sample"),
                "its remote is of type `websocket`, which fl does not know",
                "`fl mcp add sample --url <url>`",
            ),
            (
                "a value for a secret",
                plain(json!([npm(json!({
                    "environmentVariables": [
                        { "name": "SAMPLE_TOKEN", "isSecret": true, "isRequired": true }
                    ]
                }))])),
                opts_with(&|o| {
                    o.env.insert("SAMPLE_TOKEN".to_string(), VALUE.to_string());
                }),
                "`--env SAMPLE_TOKEN=…` names a secret, and a secret is never recorded",
                "Set SAMPLE_TOKEN in the environment instead",
            ),
            (
                "a required variable with no value",
                plain(json!([npm(json!({
                    "environmentVariables": [{ "name": "SAMPLE_HOME", "isRequired": true }]
                }))])),
                named("sample"),
                "its environment variable `SAMPLE_HOME` needs a value",
                "Give it with `--env SAMPLE_HOME=<value>`",
            ),
            (
                "an --env naming no variable",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.env.insert("SAMPLE_NOPE".to_string(), VALUE.to_string());
                }),
                "`--env SAMPLE_NOPE=…` names no environment variable of this launch route",
                "Drop it",
            ),
            (
                "a --with naming no argument",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.with.insert("SAMPLE_NOPE".to_string());
                }),
                "`--with SAMPLE_NOPE` names no optional argument or variable that needs a secret",
                "Drop it",
            ),
            (
                "an --env key that is no variable's name",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.env.insert(VALUE.to_string(), "x".to_string());
                }),
                "an `--env` key that is not a variable's name names no environment variable of \
                 this launch route, and fl does not repeat it",
                "Drop it",
            ),
            (
                "a --with name that is no variable's name",
                plain(json!([npm(json!({}))])),
                opts_with(&|o| {
                    o.with.insert(VALUE.to_string());
                }),
                "a `--with` name that is not a variable's name names no optional argument or \
                 variable that needs a secret, and fl does not repeat it",
                "Drop it",
            ),
            (
                "a secret header in another shape",
                header(json!({
                    "name": "Cookie", "value": "session={sid}",
                    "variables": { "sid": { "isSecret": true } }
                })),
                named("sample"),
                "its secret header `Cookie` has the value `session={sid}`, a shape fl cannot \
                 record",
                "`fl mcp add sample --url <url> --header Cookie`",
            ),
            (
                "a required header with no value",
                header(json!({ "name": "X-Tenant", "isRequired": true })),
                named("sample"),
                "its header `X-Tenant` is required, and the registry gives it no value",
                "`fl mcp add sample --url <url> --header X-Tenant`",
            ),
            (
                "an argument of a type fl does not know",
                plain(json!([npm(json!({
                    "packageArguments": [
                        { "type": "flag", "name": "--verbose", "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "its argument `--verbose` is of type `flag`, an argument type fl does not know",
                "`fl mcp add sample -- <command>`",
            ),
            (
                "a variable that cannot be named in the environment",
                plain(json!([npm(json!({
                    "environmentVariables": [
                        { "name": "sample-token", "isSecret": true, "isRequired": true }
                    ]
                }))])),
                named("sample"),
                "`sample-token` cannot be an environment variable name",
                "add the server by hand",
            ),
        ];
        let mut messages = Vec::new();
        for (what, entry, opts, phrase, next) in &cases {
            let err = freeze(entry, opts).expect_err(what);
            assert!(
                matches!(err, McpError::Unfreezable { .. }),
                "{what}: {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains("`io.example/sample` "), "{what}: {msg}");
            assert!(msg.contains(phrase), "{what}: {phrase:?} not in: {msg}");
            assert!(msg.contains(next), "{what}: {next:?} not in: {msg}");
            assert!(!msg.contains(VALUE), "{what}: a value in: {msg}");
            messages.push(msg);
        }
        for (i, (what, _, _, phrase, _)) in cases.iter().enumerate() {
            for (j, msg) in messages.iter().enumerate() {
                assert!(
                    i == j || !msg.contains(phrase),
                    "{what}'s {phrase:?} also in: {msg}"
                );
            }
        }
    }

    // The registry serves argument types its schema does not define: such
    // an argument is refused by its type, optional or not, never passed in
    // a shape fl guessed or left out unseen.
    #[test]
    fn an_argument_of_a_type_fl_does_not_know_is_refused_naming_it() {
        let verbose = fixture(fake::VERBOSE, "latest");
        let err = freeze(&verbose, &named("verbose")).unwrap_err();
        assert!(matches!(err, McpError::Unfreezable { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.starts_with("`io.example/verbose` 1.0.0 cannot be frozen into the catalog: "),
            "{msg}"
        );
        assert!(
            msg.contains(
                "its argument `--verbose` is of type `flag`, an argument type fl does not know"
            ),
            "{msg}"
        );
        assert!(msg.contains("`fl mcp add verbose -- <command>`"), "{msg}");
    }

    // MCP spec §3.3: `upgrade` freezes the new version with what the pinned
    // entry chose, where it still applies.
    #[test]
    fn upgrade_keeps_the_pinned_choices_where_they_still_apply() {
        let at = |version: &str, extra: bool| {
            let mut env = vec![json!({ "name": "SAMPLE_HOME", "isRequired": true })];
            let mut runtime = vec![json!({
                "type": "named", "name": "-e", "value": "SAMPLE_TOKEN={token}",
                "variables": { "token": { "isSecret": true } }
            })];
            if !extra {
                env.clear();
                runtime.clear();
            }
            let package = oci(json!({
                "identifier": format!("ghcr.io/example/sample-mcp:{version}"),
                "runtimeArguments": runtime,
                "environmentVariables": env
            }));
            let remote = remote(json!({}));
            let server = with(
                sample(json!([package]), json!([remote])),
                json!({ "version": version }),
            );
            response(server)
        };
        let mut opts = routed("sample", Route::Oci);
        opts.env
            .insert("SAMPLE_HOME".to_string(), "/srv/sample".to_string());
        opts.with.insert("SAMPLE_TOKEN".to_string());
        let pinned = freeze(&at("1.0.0", true), &opts).unwrap().server;

        let carried = FreezeOptions::upgrading("sample", &pinned);
        assert_eq!(carried.name, "sample");
        assert_eq!(carried.route, Some(Route::Oci));
        assert!(carried.env.is_empty() && carried.with.is_empty());
        let next = freeze(&at("1.1.0", true), &carried).unwrap().server;
        let args = [
            "run",
            "-i",
            "--rm",
            "-e",
            "SAMPLE_TOKEN",
            "-e",
            "SAMPLE_HOME",
            "ghcr.io/example/sample-mcp:1.1.0",
        ];
        assert_eq!(next.args, Some(strings(&args)));
        let env = BTreeMap::from([
            ("SAMPLE_HOME".to_string(), literal("/srv/sample")),
            ("SAMPLE_TOKEN".to_string(), secret()),
        ]);
        assert_eq!(next.env, Some(env));

        // A value named on the command line wins over the pinned one.
        let mut given = carried.clone();
        given
            .env
            .insert("SAMPLE_HOME".to_string(), "/data".to_string());
        let next = freeze(&at("1.1.0", true), &given).unwrap().server;
        assert_eq!(next.env.unwrap()["SAMPLE_HOME"], literal("/data"));

        // What the new version no longer has is dropped, not refused.
        let next = freeze(&at("1.2.0", false), &carried).unwrap().server;
        let args = ["run", "-i", "--rm", "ghcr.io/example/sample-mcp:1.2.0"];
        assert_eq!(next.args, Some(strings(&args)));
        assert_eq!(next.env, None);
    }

    // MCP spec §3.3: the difference between the pinned launch spec and the
    // new one.
    #[test]
    fn diff_lists_each_changed_field_of_the_launch_spec() {
        let old = freeze(&fixture(fake::NOTES, "1.1.0"), &named("notes")).unwrap();
        let new = freeze(&fixture(fake::NOTES, "1.2.0"), &named("notes")).unwrap();
        let shown: Vec<String> = diff(&old.server, &new.server)
            .iter()
            .map(Change::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                r#"version: "1.1.0" -> "1.2.0""#,
                concat!(
                    r#"args: ["-y", "@example/notes-mcp@1.1.0", "./notes"] -> "#,
                    r#"["-y", "@example/notes-mcp@1.2.0", "./notes"]"#
                ),
            ]
        );
        assert!(diff(&new.server, &new.server).is_empty());

        let mut changed = new.server.clone();
        let env = changed.env.as_mut().unwrap();
        env.remove("NOTES_LOG");
        env.insert("NOTES_TOKEN".to_string(), literal("x"));
        env.insert(
            "NOTES_HOME".to_string(),
            EnvValue::Secret {
                env: Some("HOME_DIR".to_string()),
            },
        );
        changed.command = Some("node".to_string());
        changed.from = Some("io.example/other".to_string());
        let shown: Vec<String> = diff(&new.server, &changed)
            .iter()
            .map(Change::to_string)
            .collect();
        assert_eq!(
            shown,
            [
                r#"from: "io.example/notes" -> "io.example/other""#,
                r#"command: "npx" -> "node""#,
                r#"env.NOTES_HOME: (none) -> { secret = true, env = "HOME_DIR" }"#,
                r#"env.NOTES_LOG: "info" -> (none)"#,
                r#"env.NOTES_TOKEN: { secret = true } -> "x""#,
            ]
        );

        let docs = freeze(&fixture(fake::DOCS, "latest"), &named("docs"))
            .unwrap()
            .server;
        let mut moved = docs.clone();
        moved.transport = Transport::Sse;
        moved.url = Some("https://docs.example.com/sse".to_string());
        let headers = moved.headers.as_mut().unwrap();
        headers.insert(
            "Authorization".to_string(),
            HeaderValue::Secret {
                env: "DOCS_TOKEN".to_string(),
                scheme: Some("Bearer".to_string()),
            },
        );
        headers.insert(
            "X-Team".to_string(),
            HeaderValue::Literal("widgets".to_string()),
        );
        let shown: Vec<String> = diff(&docs, &moved).iter().map(Change::to_string).collect();
        assert_eq!(
            shown,
            [
                r#"transport: "http" -> "sse""#,
                r#"url: "https://docs.example.com/mcp" -> "https://docs.example.com/sse""#,
                concat!(
                    r#"headers.Authorization: { secret = true, env = "DOCS_AUTHORIZATION" } -> "#,
                    r#"{ secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }"#
                ),
                r#"headers.X-Team: (none) -> "widgets""#,
            ]
        );
        let change = &diff(&docs, &moved)[0];
        assert_eq!(change.field, "transport");
        assert_eq!(change.old.as_deref(), Some(r#""http""#));
        assert_eq!(change.new.as_deref(), Some(r#""sse""#));
    }

    // MCP spec §3.3: a version that is not newer is refused unless named
    // with `--to`; versions compare by semantic-version precedence.
    #[test]
    fn an_upgrade_must_be_newer_unless_named_with_to() {
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.2.0",
            "1.10.0",
            "2.0.0",
        ];
        for pair in order.windows(2) {
            let (older, newer) = (pair[0], pair[1]);
            assert!(
                check_upgrade("s", older, newer, false).is_ok(),
                "{older} -> {newer}"
            );
            let err = check_upgrade("s", newer, older, false).unwrap_err();
            assert!(matches!(err, McpError::NotNewer { .. }), "{err:?}");
            assert!(
                err.to_string().contains("is not newer"),
                "{newer} -> {older}: {err}"
            );
            assert!(
                check_upgrade("s", newer, older, true).is_ok(),
                "--to {older}"
            );
        }
        // Build metadata does not count; the same version is not newer.
        for (pinned, offered) in [("0.9.0+build.7", "0.9.0+build.8"), ("1.2.0", "1.2.0")] {
            let err = check_upgrade("notes", pinned, offered, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("server `notes`: it is pinned to {pinned}")),
                "{msg}"
            );
            assert!(
                msg.contains(&format!("the registry's {offered} is not newer")),
                "{msg}"
            );
            assert!(
                msg.contains(&format!("`fl mcp upgrade notes --to {offered}`")),
                "{msg}"
            );
        }
        for (pinned, offered) in [
            ("1.0.0", "2026.10"),
            ("2026.10", "1.0.0"),
            ("1.0.0", "1.0.0.1"),
            ("1.0.0", "1.0.0-"),
            ("1.0.0", "1.0.0-rc..1"),
            ("1.0.0", "1.+1.0"),
            ("1.0.0", "v2.0.0"),
        ] {
            let err = check_upgrade("notes", pinned, offered, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("cannot be compared with it"),
                "{pinned} -> {offered}: {msg}"
            );
            assert!(
                msg.contains(&format!("`fl mcp upgrade notes --to {offered}`")),
                "{msg}"
            );
            assert!(check_upgrade("notes", pinned, offered, true).is_ok());
        }
    }

    /// `io.example/sample` with one package of this type, identifier and
    /// version.
    fn package_of(kind: &str, identifier: &str, version: &str) -> ServerResponse {
        let package = json!({
            "registryType": kind,
            "identifier": identifier,
            "version": version,
            "transport": { "type": "stdio" }
        });
        response(sample(json!([package]), json!([])))
    }

    fn refusal(resp: &ServerResponse) -> String {
        freeze(resp, &named("s")).unwrap_err().to_string()
    }

    // `upgrade` has no `--package` or `--remote`: when the pinned route is
    // gone from the new version, the refusal names the commands that can be
    // followed, not "choose one".
    #[test]
    fn an_upgrade_whose_pinned_route_is_gone_names_remove_then_add() {
        let pinned = freeze(
            &response(sample(json!([]), json!([remote(json!({}))]))),
            &named("sample"),
        )
        .unwrap()
        .server;
        let carried = FreezeOptions::upgrading("sample", &pinned);
        let gone = response(sample(json!([npm(json!({}))]), json!([])));
        let msg = freeze(&gone, &carried).unwrap_err().to_string();
        assert!(
            msg.contains("offers no `--remote` route, only `--package npm`"),
            "{msg}"
        );
        assert!(
            msg.contains(
                "`fl mcp remove sample`, then `fl mcp add sample --from io.example/sample \
                 --package <type>` or `--remote`"
            ),
            "{msg}"
        );
        assert!(!msg.contains("Choose one it offers"), "{msg}");
        // `add` has the flags: its refusal is unchanged.
        let msg = freeze(&gone, &routed("sample", Route::Remote))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("Choose one it offers"), "{msg}");
    }

    // `status` is an open string: one fl does not know is refused at freeze,
    // naming it and what to do, and its text reaches the message through
    // `printable`.
    #[test]
    fn a_status_fl_does_not_know_is_refused_naming_it() {
        let server = || sample(json!([]), json!([remote(json!({}))]));
        let msg = freeze(&response_with(server(), "archived", None), &named("s"))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("its status in the registry is `archived`, one fl does not know"),
            "{msg}"
        );
        assert!(msg.contains("Choose another version or server"), "{msg}");
        assert!(msg.contains("fl mcp add s -- <command>"), "{msg}");
        let hostile = "arch\u{1b}]0;pwn\u{7}\u{202e}ived";
        let msg = freeze(&response_with(server(), hostile, None), &named("s"))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("its status in the registry is `arch]0;pwnived`"),
            "{msg}"
        );
        assert!(!msg.chars().any(crate::registry::is_unseen), "{msg:?}");
        let long = "x".repeat(400);
        let msg = freeze(&response_with(server(), &long, None), &named("s"))
            .unwrap_err()
            .to_string();
        assert!(!msg.contains(&"x".repeat(301)), "{msg}");
        // The statuses fl knows are not refused for their status.
        for status in ["active", "deprecated"] {
            freeze(&response_with(server(), status, None), &named("s")).unwrap();
        }
    }

    // MCP spec §2.1: the pin is exact. A range, a wildcard, a tag, a URL or an
    // option in a package's version or name would let the registry (or the
    // package manager) choose what runs.
    #[test]
    fn an_npm_or_pypi_package_pins_an_exact_version_of_a_plain_name() {
        let accepted = [
            ("npm", "@example/sample-mcp", "1.2.3"),
            ("npm", "sample.mcp_x~y", "0.0.0"),
            ("npm", "@a/b", "10.20.30-beta.1+build-5.x"),
            ("npm", "@a1/b2", "1.0.0"),
            ("pypi", "Example_Sample.mcp-2", "1.0.0"),
            ("pypi", "x", "2!1.0.0rc1+local.1"),
        ];
        for (kind, id, version) in accepted {
            let resp = package_of(kind, id, version);
            let frozen = freeze(&resp, &named("s"));
            assert!(frozen.is_ok(), "{kind} {id} {version}: {frozen:?}");
        }

        let npm_versions = [
            "^1.2.0",
            "~1.2.3",
            ">=1.0.0",
            "1.*",
            "next",
            "1.2",
            "01.2.3",
            "1.02.3",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-a..b",
            "1.2.3-beta_1",
            "1..3",
            "1.2.",
            "1.a.3",
            "1.2.3 ",
            "1.2.3.4",
            "v1.2.3",
        ];
        for version in npm_versions {
            let msg = refusal(&package_of("npm", "@example/x", version));
            assert!(
                msg.contains("which is not an exact npm version"),
                "{version}: {msg}"
            );
        }
        let pypi_versions = ["1.*", "*", "next", "==1.0", ">=1", "1.0 ", "1.0;x", "v1"];
        for version in pypi_versions {
            let msg = refusal(&package_of("pypi", "example-x", version));
            assert!(
                msg.contains("which is not an exact PyPI version"),
                "{version}: {msg}"
            );
        }

        let npm_names = [
            "https://evil.example/x.tgz#",
            "git+https://example.com/x.git",
            "-x",
            ".x",
            "@scope",
            "@/x",
            "@scope/",
            "@-s/x",
            "@.s/x",
            "@a/b/c",
            "a/b",
            "Sample",
            "x y",
            "",
        ];
        for id in npm_names {
            let msg = refusal(&package_of("npm", id, "1.0.0"));
            assert!(
                msg.contains("is not a valid npm package name"),
                "{id}: {msg}"
            );
        }
        let pypi_names = [
            "-x",
            "_x",
            ".x",
            "x y",
            "x==1",
            "https://example.com/x",
            "x/y",
            "",
        ];
        for id in pypi_names {
            let msg = refusal(&package_of("pypi", id, "1.0.0"));
            assert!(
                msg.contains("is not a valid PyPI package name"),
                "{id}: {msg}"
            );
        }

        // An image name that starts with `-` is an option to `docker run`.
        let package = oci(json!({ "identifier": "--volume=/:/host:rw" }));
        let msg = refusal(&response(sample(json!([package]), json!([]))));
        assert!(
            msg.contains("starts with `-`, which docker would read as an option"),
            "{msg}"
        );
    }

    // A variable's name comes from the registry and is shown in a message:
    // never raw.
    #[test]
    fn a_variable_name_is_shown_without_its_control_characters() {
        let esc = "\u{1b}[2Jtok";
        let url_secret = remote(json!({
            "url": format!("https://x.example/{{{esc}}}"),
            "variables": { esc: { "isSecret": true } }
        }));
        let url_unset = remote(json!({
            "url": format!("https://x.example/{{{esc}}}"),
            "variables": { esc: {} }
        }));
        let argument = npm(json!({
            "packageArguments": [{
                "type": "positional", "value": format!("{{{esc}}}"), "isRequired": true,
                "variables": { esc: {} }
            }]
        }));
        let cases = [
            (
                sample(json!([]), json!([url_secret])),
                "names `{[2Jtok}`, which is a secret",
            ),
            (
                sample(json!([]), json!([url_unset])),
                "names `{[2Jtok}`, which has neither",
            ),
            (
                sample(json!([argument]), json!([])),
                "names an unset variable, `{[2Jtok}`",
            ),
        ];
        for (server, expected) in cases {
            let msg = refusal(&response(server));
            assert!(msg.contains(expected), "{msg:?}");
            assert!(!msg.chars().any(char::is_control), "{msg:?}");
        }
    }

    // MCP spec §4.2: a header name goes into a vendor file and onto the wire.
    #[test]
    fn a_header_name_must_be_an_http_token() {
        for name in ["X A", "X:A", "X(A)", "X/A", "X\"A", "Ä-A", ""] {
            let header = json!({ "name": name, "value": "v" });
            let server = sample(json!([]), json!([remote(json!({ "headers": [header] }))]));
            let msg = refusal(&response(server));
            assert!(
                msg.contains("is not an HTTP header name, a shape fl cannot record"),
                "{name}: {msg}"
            );
        }
        let name = "Authorization-X_1.0!#$%&'*+^`|~";
        let header = json!({ "name": name, "value": "v" });
        let server = sample(json!([]), json!([remote(json!({ "headers": [header] }))]));
        let frozen = freeze(&response(server), &named("s")).unwrap();
        let headers = frozen.server.headers.unwrap();
        assert_eq!(headers[name], HeaderValue::Literal("v".to_string()));
    }

    // Every registry string fl records is refused when it holds a control,
    // format or invisible character: it would reach the committed catalog,
    // and a bidirectional override makes a line read as another.
    #[test]
    fn a_registry_string_with_a_control_or_invisible_character_is_refused() {
        let bidi = "\u{202e}";
        let with_package = |extra: Value| sample(json!([npm(extra)]), json!([]));
        let with_header =
            |header: Value| sample(json!([]), json!([remote(json!({ "headers": [header] }))]));
        let cases = [
            (
                with(
                    sample(json!([npm(json!({}))]), json!([])),
                    json!({ "name": "io.example/s\u{202e}" }),
                ),
                "its server name",
            ),
            (
                with(
                    sample(json!([npm(json!({}))]), json!([])),
                    json!({ "version": "1.0.0\u{1b}[2J\u{202e}" }),
                ),
                "its version",
            ),
            (
                with_package(json!({ "identifier": format!("@example/sample{bidi}") })),
                "its package identifier",
            ),
            (
                sample(
                    json!([oci(
                        json!({ "identifier": "ghcr.io/example/x\u{200b}:1.0.0" })
                    )]),
                    json!([]),
                ),
                "its package identifier",
            ),
            (
                with_package(json!({ "version": format!("1.0.0{bidi}") })),
                "its package version",
            ),
            (
                with_package(json!({ "packageArguments": [
                    { "type": "positional", "value": format!("a{bidi}b") }
                ] })),
                "an argument it passes",
            ),
            (
                with_package(json!({ "environmentVariables": [
                    { "name": "SAMPLE_X", "default": "v\u{2066}" }
                ] })),
                "its environment variable `SAMPLE_X`",
            ),
            (
                with_package(json!({ "environmentVariables": [
                    { "name": format!("SAMPLE{bidi}"), "default": "v" }
                ] })),
                "its environment variable `SAMPLE`",
            ),
            (
                sample(
                    json!([]),
                    json!([remote(
                        json!({ "url": format!("https://sample.example.com/m{bidi}cp") })
                    )]),
                ),
                "its remote URL",
            ),
            (
                with_header(json!({ "name": "X-A\u{1b}[2J\r\nInjected: 1", "value": "v" })),
                "its header name",
            ),
            (
                with_header(json!({ "name": "X-A", "value": "v\u{feff}" })),
                "its header `X-A`",
            ),
        ];
        for (server, what) in cases {
            let msg = refusal(&response(server));
            let expected = format!("{what} holds a control or invisible character");
            assert!(msg.contains(&expected), "{what}: {msg:?}");
            assert!(!msg.chars().any(crate::registry::is_unseen), "{msg:?}");
        }
    }
}
