//! Filesystem-backed startup guidance. Logical project names are never read paths.
use std::path::{Path, PathBuf};

use fl_core::store::Catalog;
use fl_mcp::project_paths::resolve_git_project;

use crate::project_init::{config_entries, read_config, user_config_path};

pub(crate) struct HydrationPrompt {
    pub text: String,
    pub warnings: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Context {
    workspace: Option<PathBuf>,
    project: Option<PathBuf>,
    warnings: Vec<String>,
}

pub(crate) enum RoleGuidance<'a> {
    File,
    Catalog(&'a str),
    StructuralCustom,
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().filter(|p| p.is_dir())
}

fn matching_dirs(parent: &Path, name: &str) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .into_iter().flatten().flatten()
        .filter(|entry| entry.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .filter_map(|entry| canonical_dir(&entry.path()))
        .collect()
}

fn unique(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

impl Context {
    pub(crate) fn resolve<S: Catalog>(store: &S, home: Option<&Path>, workspace: Option<&Path>, cwd: Option<&Path>, name: &str) -> Self {
        let mut context = Self { workspace: workspace.and_then(canonical_dir), ..Self::default() };
        let mut roots = Vec::new();
        let mut bound = false;
        if let Some(config) = user_config_path(home) {
            match read_config(&config).and_then(|doc| config_entries(&doc, &config)) {
                Ok(entries) => {
                    for entry in entries.iter().filter(|e| e.slug().is_some_and(|slug| slug.eq_ignore_ascii_case(name))) {
                        bound = true;
                        if let Some(root) = canonical_dir(Path::new(&entry.root)) { roots.push(root); }
                        else { context.warnings.push(format!("registered project root unavailable for {name}")); }
                    }
                }
                Err(_) => {
                    // TOML errors can echo credentials or other private config contents.
                    context.warnings.push("project bindings unavailable: could not read or parse project configuration".into());
                    bound = true;
                }
            }
        }
        // Config names are authoritative; catalog roots have only their directory name.
        if !bound {
            match store.list_projects() {
                Ok(projects) => for project in projects {
                    let path = Path::new(project.root.trim());
                    if path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name)) {
                        bound = true;
                        if let Some(root) = canonical_dir(path) { roots.push(root); }
                        else { context.warnings.push(format!("registered project root unavailable for {name}")); }
                    }
                },
                Err(_) => {
                    bound = true;
                    context.warnings.push("project catalog unavailable".into());
                }
            }
        }
        if context.workspace.is_none() {
            context.workspace = roots.iter().find_map(|root| fl_core::find_workspace_root(root))
                .or_else(|| cwd.and_then(fl_core::find_workspace_root)).and_then(|p| canonical_dir(&p));
        }
        if !bound && let Some(workspace) = &context.workspace { roots = matching_dirs(workspace, name); }
        let roots = unique(roots);
        if roots.len() > 1 { context.warnings.push(format!("ambiguous project roots for {name}; project guidance unavailable")); }
        else { context.project = roots.into_iter().next(); }
        if let (Some(root), Some(cwd)) = (&context.project, cwd) {
            let registered = resolve_git_project(root);
            let current = resolve_git_project(cwd);
            match (registered, current) {
                (Ok(Some(registered)), Ok(Some(current))) if registered.worktree_root == *root && registered.common_dir == current.common_dir => {
                    context.project = Some(current.worktree_root);
                }
                (Ok(None), Ok(None)) if canonical_dir(cwd).is_some_and(|p| p.starts_with(root)) => {}
                _ => context.warnings.push(format!("cwd is outside project {name} or its repository identity is unavailable; using registered project guidance")),
            }
        }
        context
    }

    fn required(&mut self, path: Option<PathBuf>, label: &str) -> String {
        if let Some(path) = path.as_deref().and_then(regular_file) {
            return format!("Read `{}`.", path.display());
        }
        let warning = match path {
            Some(path) => format!("{label} not found: {}", path.display()),
            None => format!("{label} unavailable: root could not be established"),
        };
        self.warnings.push(warning.clone());
        format!("Missing guidance: {warning}. Continue with available guidance.")
    }
}

fn regular_file(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().filter(|p| p.is_file())
}

pub(crate) fn render(mut context: Context, name: &str, project: &str, role: &str, guidance: RoleGuidance<'_>) -> HydrationPrompt {
    let residence = context.project.as_ref().map(|p| format!("`{}`", p.display())).unwrap_or_else(|| "the unresolved project context".into());
    let workspace = context.required(context.workspace.as_ref().map(|p| p.join(".agents/AGENTS.md")), "workspace AGENTS.md");
    let role_guidance = match guidance {
        RoleGuidance::StructuralCustom => "Retain your custom role identity; it grants no structural authority.".into(),
        RoleGuidance::Catalog(template) => {
            let base = context.workspace.as_ref().map(|p| format!("`{}`", p.display())).unwrap_or_else(|| "unavailable (workspace root could not be established)".into());
            format!("Catalog guidance workspace-relative base: {base}.\nApply your current catalog role guidance: {template}")
        }
        RoleGuidance::File => {
            let file_role = match role { "web-developer" | "mobile-developer" => "ui-developer", other => other };
            let mut candidates = Vec::new();
            if let Some(ws) = &context.workspace {
                let roles = ws.join(".agents/roles");
                let dirs = unique(matching_dirs(&roles, project));
                if dirs.len() > 1 {
                    context.warnings.push(format!("ambiguous project role directories for {project}; using global role guidance"));
                } else if let Some(dir) = dirs.first() { candidates.push(dir.join(format!("{file_role}.md"))); }
                else { candidates.push(roles.join(project).join(format!("{file_role}.md"))); }
                candidates.push(roles.join(format!("{file_role}.md")));
            }
            if let Some(path) = candidates.iter().find_map(|p| regular_file(p)) {
                format!("Read your role file(s): `{}`.", path.display())
            } else {
                let locations = if candidates.is_empty() { "workspace unavailable".into() } else { candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ") };
                let warning = format!("role file not found: looked in {locations}");
                context.warnings.push(warning.clone());
                format!("Missing guidance: {warning}. Continue with available guidance.")
            }
        }
    };
    let instructions = ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"].map(|file| context.required(context.project.as_ref().map(|p| p.join(file)), &format!("project {file}"))).join("\n");
    let legacy = context.workspace.as_ref().and_then(|ws| {
        let dirs = unique(matching_dirs(&ws.join("Notes"), project));
        (dirs.len() == 1).then(|| regular_file(&dirs[0].join("STATE.md"))).flatten()
    }).map(|path| format!("Optional legacy reference: skim `{}`; FerroState wins wherever the two disagree. Do not require it.\n", path.display())).unwrap_or_default();
    let name_lower = name.to_ascii_lowercase();
    let scope = crate::api::project_state_scope(project);
    let render_scopes = if scope == "workspace" { "--scope workspace".into() } else { format!("--scope workspace --scope {scope}") };
    let text = format!(
        "Take the name of {name} and take up residence in {residence} as the {role}.\n\n\
         This session has just been reset (`/clear`) for clean-slate context hygiene. Perform your root startup hydration now:\n\
         1. {workspace} Check the active roster with `fl agent roster`.\n\
         2. {role_guidance}\n\
         3. {instructions}\n\
         4. Read your working state from FerroState, the primary state source: run `fl state render {render_scopes}` for the startup summary, `fl state handoff get --scope {scope} --key {name_lower}` for your own latest handoff (if it reports none, `fl state handoff get --scope {scope}` reads the shared `current` handoff), and `fl state search \"<query>\"` to look up anything specific.\n\
         {legacy}\
         5. Re-register your active session route so FerroWire and FerroDeck track this fresh session:\n\
         ```bash\nAGENT_PERSONA={name_lower} fl wire register {name} --project {project} --role {role} --replace\n```\n\
         6. Check and acknowledge any pending FerroWire messages:\n\
         ```bash\nAGENT_PERSONA={name_lower} fl wire read --ack\n```\n\n\
         Identity summary: `{name}` | Co-author trailer: `Co-authored-by: {name} <{name_lower}@artificialhumanity.io>`"
    );
    HydrationPrompt { text, warnings: context.warnings }
}

pub(crate) fn log_warnings(persona: &str, session_id: &str, warnings: &[String]) {
    for warning in warnings {
        let line = format!("hydration warning persona={persona} session={session_id}: {warning}");
        eprintln!("{line}");
        #[cfg(test)]
        WARNING_LOG.with_borrow_mut(|lines| lines.push(line));
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static WARNING_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
