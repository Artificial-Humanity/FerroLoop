//! Filesystem-backed startup guidance. Logical project names are never read paths.
use std::path::{Path, PathBuf};

use fl_core::store::Catalog;
use fl_mcp::project_paths::{GitProjectLayout, resolve_git_project};

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

fn registered_dir(path: &Path) -> Option<PathBuf> {
    // A binding has no specified base for relative paths. Do not let the hub's
    // process cwd accidentally authorize another repository's instructions.
    path.is_absolute().then(|| canonical_dir(path)).flatten()
}

fn matching_dirs(parent: &Path, name: &str) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
        })
        .filter_map(|entry| canonical_dir(&entry.path()))
        .collect()
}

fn unique(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) type ProjectRoots = Result<Vec<String>, ()>;

pub(crate) fn project_roots(store: &impl Catalog) -> ProjectRoots {
    store
        .list_projects()
        .map(|projects| projects.into_iter().map(|project| project.root).collect())
        .map_err(|_| ())
}

fn corresponding_project(
    root: &Path,
    registered: &GitProjectLayout,
    current: &GitProjectLayout,
) -> Option<PathBuf> {
    if registered.common_dir != current.common_dir {
        return None;
    }
    let suffix = root.strip_prefix(&registered.worktree_root).ok()?;
    let candidate = current.worktree_root.join(suffix);
    let resolved = canonical_dir(&candidate)?;
    // A corresponding subproject must exist at the same relative location. Do not
    // let a worktree symlink redirect it to a different project or repository.
    if resolved != candidate || !resolved.starts_with(&current.worktree_root) {
        return None;
    }
    let layout = resolve_git_project(&resolved).ok()??;
    (layout.common_dir == current.common_dir && layout.worktree_root == current.worktree_root)
        .then_some(resolved)
}

impl Context {
    pub(crate) fn resolve<S: Catalog>(
        store: &S,
        home: Option<&Path>,
        workspace: Option<&Path>,
        cwd: Option<&Path>,
        name: &str,
    ) -> Self {
        Self::resolve_with_catalog(project_roots(store), home, workspace, cwd, name)
    }

    pub(crate) fn resolve_with_catalog(
        projects: ProjectRoots,
        home: Option<&Path>,
        workspace: Option<&Path>,
        cwd: Option<&Path>,
        name: &str,
    ) -> Self {
        #[cfg(test)]
        if let Some(hook) = RESOLVE_HOOK.with_borrow_mut(Option::take) {
            hook();
        }
        let mut context = Self {
            workspace: workspace.and_then(canonical_dir),
            ..Self::default()
        };
        let mut roots = Vec::new();
        let mut bound = false;
        if let Some(config) = user_config_path(home) {
            match read_config(&config).and_then(|doc| config_entries(&doc, &config)) {
                Ok(entries) => {
                    for entry in entries
                        .iter()
                        .filter(|e| e.slug().is_some_and(|slug| slug.eq_ignore_ascii_case(name)))
                    {
                        bound = true;
                        if let Some(root) = registered_dir(Path::new(&entry.root)) {
                            roots.push(root);
                        } else {
                            context
                                .warnings
                                .push(format!("registered project root unavailable for {name}"));
                        }
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
            match projects {
                Ok(projects) => {
                    for project in projects {
                        let path = Path::new(project.trim());
                        if path
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
                        {
                            bound = true;
                            if let Some(root) = registered_dir(path) {
                                roots.push(root);
                            } else {
                                context.warnings.push(format!(
                                    "registered project root unavailable for {name}"
                                ));
                            }
                        }
                    }
                }
                Err(_) => {
                    bound = true;
                    context.warnings.push("project catalog unavailable".into());
                }
            }
        }
        if context.workspace.is_none() {
            // Registered roots establish provenance. An unrelated cwd must never supply
            // workspace instructions for a bound project, nor may first-match ordering
            // choose among conflicting workspaces.
            let mut workspaces = unique(
                roots
                    .iter()
                    .filter_map(|root| fl_core::find_workspace_root(root))
                    .filter_map(|root| canonical_dir(&root))
                    .collect(),
            );
            if workspaces.len() == 1 {
                context.workspace = workspaces.pop();
            } else if workspaces.len() > 1 {
                context.warnings.push(format!(
                    "ambiguous workspace roots for {name}; workspace guidance unavailable"
                ));
            } else if !bound {
                context.workspace = cwd
                    .and_then(fl_core::find_workspace_root)
                    .and_then(|root| canonical_dir(&root));
            }
        }
        if !bound && let Some(workspace) = &context.workspace {
            roots = matching_dirs(workspace, name);
        }
        let roots = unique(roots);
        if roots.len() > 1 {
            context.warnings.push(format!(
                "ambiguous project roots for {name}; project guidance unavailable"
            ));
        } else {
            context.project = roots.into_iter().next();
        }
        if let (Some(root), Some(cwd)) = (&context.project, cwd) {
            let registered = resolve_git_project(root);
            let current = resolve_git_project(cwd);
            match (registered, current) {
                (Ok(Some(registered)), Ok(Some(current))) => {
                    if let Some(selected) = corresponding_project(root, &registered, &current) {
                        context.project = Some(selected);
                    } else {
                        context.warnings.push(format!("cwd is outside project {name} or its corresponding project directory is unavailable; using registered project guidance"));
                    }
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

pub(crate) fn render(
    mut context: Context,
    name: &str,
    project: &str,
    role: &str,
    guidance: RoleGuidance<'_>,
) -> HydrationPrompt {
    let residence = context
        .project
        .as_ref()
        .map(|p| format!("`{}`", p.display()))
        .unwrap_or_else(|| "the unresolved project context".into());
    let workspace = context.required(
        context
            .workspace
            .as_ref()
            .map(|p| p.join(".agents/AGENTS.md")),
        "workspace AGENTS.md",
    );
    let role_guidance = match guidance {
        RoleGuidance::StructuralCustom => {
            "Retain your custom role identity; it grants no structural authority.".into()
        }
        RoleGuidance::Catalog(template) => {
            let base = context
                .workspace
                .as_ref()
                .map(|p| format!("`{}`", p.display()))
                .unwrap_or_else(|| "unavailable (workspace root could not be established)".into());
            format!(
                "Catalog guidance workspace-relative base: {base}.\nApply your current catalog role guidance: {template}"
            )
        }
        RoleGuidance::File => {
            let file_role = match role {
                "web-developer" | "mobile-developer" => "ui-developer",
                other => other,
            };
            let mut candidates = Vec::new();
            if let Some(ws) = &context.workspace {
                let roles = ws.join(".agents/roles");
                let dirs = unique(matching_dirs(&roles, project));
                if dirs.len() > 1 {
                    context.warnings.push(format!("ambiguous project role directories for {project}; using global role guidance"));
                } else if let Some(dir) = dirs.first() {
                    candidates.push(dir.join(format!("{file_role}.md")));
                } else {
                    candidates.push(roles.join(project).join(format!("{file_role}.md")));
                }
                candidates.push(roles.join(format!("{file_role}.md")));
            }
            if let Some(path) = candidates.iter().find_map(|p| regular_file(p)) {
                format!("Read your role file(s): `{}`.", path.display())
            } else {
                let locations = if candidates.is_empty() {
                    "workspace unavailable".into()
                } else {
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let warning = format!("role file not found: looked in {locations}");
                context.warnings.push(warning.clone());
                format!("Missing guidance: {warning}. Continue with available guidance.")
            }
        }
    };
    let instructions = ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"]
        .map(|file| {
            context.required(
                context.project.as_ref().map(|p| p.join(file)),
                &format!("project {file}"),
            )
        })
        .join("\n");
    let legacy = context.workspace.as_ref().and_then(|ws| {
        let dirs = unique(matching_dirs(&ws.join("Notes"), project));
        (dirs.len() == 1).then(|| regular_file(&dirs[0].join("STATE.md"))).flatten()
    }).map(|path| format!("Optional legacy reference: skim `{}`; FerroState wins wherever the two disagree. Do not require it.\n", path.display())).unwrap_or_default();
    let name_lower = name.to_ascii_lowercase();
    let scope = crate::api::project_state_scope(project);
    let render_scopes = if scope == "workspace" {
        "--scope workspace".into()
    } else {
        format!("--scope workspace --scope {scope}")
    };
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
    HydrationPrompt {
        text,
        warnings: context.warnings,
    }
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
    pub(crate) static RESOLVE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static WARNING_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use tempfile::TempDir;

    struct Fixture {
        home: TempDir,
        workspace: PathBuf,
        project: PathBuf,
        store: MemStore,
    }

    impl Fixture {
        fn new() -> Self {
            let home = TempDir::new().unwrap();
            let workspace = home.path().join("workspace");
            let project = workspace.join("Actual-Checkout");
            std::fs::create_dir_all(project.join("src")).unwrap();
            std::fs::create_dir_all(workspace.join(".agents/roles")).unwrap();
            std::fs::create_dir_all(workspace.join("Notes")).unwrap();
            for file in ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"] {
                std::fs::write(project.join(file), "project").unwrap();
            }
            std::fs::write(workspace.join(".agents/AGENTS.md"), "workspace").unwrap();
            std::fs::write(workspace.join(".agents/roles/assistant.md"), "global").unwrap();
            let fixture = Self {
                home,
                workspace: workspace.canonicalize().unwrap(),
                project: project.canonicalize().unwrap(),
                store: MemStore::default(),
            };
            fixture.bind(std::slice::from_ref(&fixture.project));
            fixture
        }
        fn bind(&self, roots: &[PathBuf]) {
            let config = self.home.path().join(".config/fl");
            std::fs::create_dir_all(&config).unwrap();
            let entries: String = roots
                .iter()
                .map(|root| {
                    format!(
                        "[[project]]\nname = 'logical-name'\nroot = {:?}\n",
                        root.to_str().unwrap()
                    )
                })
                .collect();
            std::fs::write(config.join("config.toml"), entries).unwrap();
        }
        fn context(&self, cwd: &Path) -> Context {
            Context::resolve(
                &self.store,
                Some(self.home.path()),
                None,
                Some(cwd),
                "logical-name",
            )
        }
        fn prompt(&self, cwd: &Path, role: &str, guidance: RoleGuidance<'_>) -> HydrationPrompt {
            render(self.context(cwd), "Ada", "logical-name", role, guidance)
        }
    }

    #[test]
    fn hydration_non_git_registered_subdirectory_uses_exact_binding_and_workspace() {
        let f = Fixture::new();
        let result = f.prompt(&f.project.join("src"), "assistant", RoleGuidance::File);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        for path in [
            f.project.join("AGENTS.md"),
            f.workspace.join(".agents/AGENTS.md"),
            f.workspace.join(".agents/roles/assistant.md"),
        ] {
            assert!(
                result.text.contains(&format!("`{}`", path.display())),
                "{}",
                result.text
            );
        }
        assert!(result.text.contains("--project logical-name"));
        assert!(result.text.contains("--scope project:logical-name"));
        assert!(!result.text.contains("STATE.md"));
    }

    fn subproject_fixture() -> (Fixture, PathBuf, PathBuf) {
        let f = Fixture::new();
        let repo = f.project.parent().unwrap().to_path_buf();
        std::fs::create_dir(repo.join(".git")).unwrap();
        let worktree = f.home.path().join("linked");
        let gitdir = repo.join(".git/worktrees/linked");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", gitdir.display()),
        )
        .unwrap();
        std::fs::write(gitdir.join("commondir"), "../..").unwrap();
        let subproject = worktree.join("Actual-Checkout");
        std::fs::create_dir(&subproject).unwrap();
        for file in ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"] {
            std::fs::write(subproject.join(file), "worktree subproject").unwrap();
        }
        (f, worktree, subproject)
    }

    #[test]
    fn hydration_subproject_selects_only_corresponding_same_repository_guidance() {
        let (f, worktree, subproject) = subproject_fixture();
        for (cwd, expected) in [
            (&f.project, &f.project),
            (&worktree, &subproject),
            (&subproject, &subproject),
        ] {
            let context = f.context(cwd);
            assert_eq!(context.project.as_ref(), Some(expected));
            assert!(context.warnings.is_empty(), "{:?}", context.warnings);
        }
        std::fs::remove_dir_all(&subproject).unwrap();
        let context = f.context(&worktree);
        assert_eq!(context.project.as_ref(), Some(&f.project));
        assert!(!context.warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hydration_subproject_rejects_symlink_escape_and_nested_foreign_repository() {
        let (f, worktree, subproject) = subproject_fixture();
        let foreign = f.home.path().join("foreign");
        std::fs::create_dir_all(foreign.join(".git")).unwrap();
        std::fs::write(foreign.join("AGENTS.md"), "foreign").unwrap();
        std::fs::remove_dir_all(&subproject).unwrap();
        std::os::unix::fs::symlink(&foreign, &subproject).unwrap();
        for cwd in [&worktree, &foreign] {
            let context = f.context(cwd);
            assert_eq!(context.project.as_ref(), Some(&f.project));
            assert!(!context.warnings.is_empty());
        }
        std::fs::remove_file(&subproject).unwrap();
        std::fs::create_dir_all(subproject.join(".git")).unwrap();
        let context = f.context(&worktree);
        assert_eq!(context.project.as_ref(), Some(&f.project));
        assert!(!context.warnings.is_empty());
    }

    #[test]
    fn hydration_foreign_same_name_and_malformed_git_never_override_registered_guidance() {
        let f = Fixture::new();
        std::fs::create_dir(f.project.join(".git")).unwrap();
        let foreign = f.home.path().join("foreign/Actual-Checkout");
        std::fs::create_dir_all(foreign.join(".git")).unwrap();
        std::fs::write(foreign.join("AGENTS.md"), "foreign").unwrap();
        let malformed = f.home.path().join("malformed");
        std::fs::create_dir(&malformed).unwrap();
        std::fs::write(malformed.join(".git"), "gitdir: missing").unwrap();
        for cwd in [&foreign, &malformed] {
            let result = f.prompt(cwd, "assistant", RoleGuidance::File);
            assert!(
                result
                    .text
                    .contains(&format!("Read `{}`", f.project.join("AGENTS.md").display()))
            );
            assert!(
                !result
                    .text
                    .contains(&format!("Read `{}`", cwd.join("AGENTS.md").display()))
            );
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|w| w.contains("cwd is outside project logical-name"))
            );
        }
    }

    #[test]
    fn hydration_ambiguous_bindings_do_not_choose_project_or_workspace() {
        let f = Fixture::new();
        let other_workspace = f.home.path().join("other");
        let other = other_workspace.join("Actual-Checkout");
        std::fs::create_dir_all(other_workspace.join("Notes")).unwrap();
        std::fs::create_dir(&other).unwrap();
        f.bind(&[f.project.clone(), other]);
        let context = f.context(f.home.path());
        assert!(context.project.is_none());
        assert!(
            context.workspace.is_none(),
            "ambiguous bindings must not choose the first workspace"
        );
        assert!(
            context
                .warnings
                .iter()
                .any(|w| w.contains("ambiguous project roots"))
        );
    }

    #[test]
    fn hydration_known_root_never_borrows_foreign_cwd_workspace() {
        let f = Fixture::new();
        let standalone = f.home.path().join("standalone");
        std::fs::create_dir(&standalone).unwrap();
        f.bind(std::slice::from_ref(&standalone));
        let context = f.context(&f.project);
        assert_eq!(context.project, Some(standalone.canonicalize().unwrap()));
        assert!(
            context.workspace.is_none(),
            "unrelated cwd must not supply workspace guidance"
        );
    }

    #[test]
    fn hydration_duplicate_bindings_deduplicate_and_catalog_fallback_is_case_aware() {
        let f = Fixture::new();
        f.bind(&[f.project.clone(), f.project.clone()]);
        assert!(
            f.prompt(&f.project, "assistant", RoleGuidance::File)
                .warnings
                .is_empty()
        );
        f.bind(&[]);
        f.store.add_project(f.project.to_str().unwrap()).unwrap();
        let context = Context::resolve(
            &f.store,
            Some(f.home.path()),
            None,
            Some(&f.project),
            "actual-checkout",
        );
        assert_eq!(context.project, Some(f.project));
    }

    #[test]
    fn hydration_relative_binding_never_resolves_against_the_hub_process_cwd() {
        let f = Fixture::new();
        f.bind(&[PathBuf::from(".")]);
        let context = f.context(&f.project);
        assert!(
            context.project.is_none(),
            "relative configuration must not authorize the hub checkout"
        );
        assert!(
            context
                .warnings
                .iter()
                .any(|w| w.contains("registered project root unavailable"))
        );
    }

    #[test]
    fn hydration_ambiguous_catalog_and_role_directories_fall_back_without_guessing() {
        let f = Fixture::new();
        f.bind(&[]);
        let other = f.home.path().join("other/Actual-Checkout");
        std::fs::create_dir_all(&other).unwrap();
        for root in [&f.project, &other] {
            f.store.add_project(root.to_str().unwrap()).unwrap();
        }
        let context = Context::resolve(
            &f.store,
            Some(f.home.path()),
            Some(&f.workspace),
            Some(&f.project),
            "actual-checkout",
        );
        assert!(context.project.is_none());
        assert!(
            context
                .warnings
                .iter()
                .any(|w| w.contains("ambiguous project roots"))
        );
        let roles = f.workspace.join(".agents/roles");
        for name in ["Logical-Name", "LOGICAL-NAME"] {
            std::fs::create_dir_all(roles.join(name)).unwrap();
            std::fs::write(roles.join(name).join("assistant.md"), "ambiguous").unwrap();
        }
        // On case-insensitive filesystems these names are the same directory.
        if unique(matching_dirs(&roles, "logical-name")).len() != 2 {
            return;
        }
        f.bind(std::slice::from_ref(&f.project));
        let result = f.prompt(&f.project, "assistant", RoleGuidance::File);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("ambiguous project role directories"))
        );
        assert!(result.text.contains(&format!(
            "Read your role file(s): `{}`",
            roles.join("assistant.md").display()
        )));
    }

    #[test]
    fn hydration_missing_binding_and_parse_errors_do_not_guess_roots_or_echo_contents() {
        let f = Fixture::new();
        f.bind(&[f.home.path().join("absent")]);
        assert!(f.context(&f.project).project.is_none());
        std::fs::write(
            f.home.path().join(".config/fl/config.toml"),
            "secret = 'PRIVATE_TOKEN\n",
        )
        .unwrap();
        let result = f.prompt(&f.project, "assistant", RoleGuidance::File);
        assert!(!result.text.contains("PRIVATE_TOKEN"));
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("could not read or parse"))
        );
        assert!(result.text.contains("project AGENTS.md unavailable"));
    }

    #[test]
    fn hydration_project_role_precedes_global_and_optional_state_is_present_only_when_found() {
        let f = Fixture::new();
        let role = f.workspace.join(".agents/roles/Logical-Name/assistant.md");
        std::fs::create_dir_all(role.parent().unwrap()).unwrap();
        std::fs::write(&role, "specific").unwrap();
        let state = f.workspace.join("Notes/Logical-Name/STATE.md");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::write(&state, "legacy").unwrap();
        let result = f.prompt(&f.project, "assistant", RoleGuidance::File);
        assert!(result.warnings.is_empty());
        assert!(
            result
                .text
                .contains(&format!("Read your role file(s): `{}`", role.display()))
        );
        assert!(
            !result.text.contains(
                &f.workspace
                    .join(".agents/roles/assistant.md")
                    .display()
                    .to_string()
            )
        );
        assert!(result.text.contains(&format!(
            "Optional legacy reference: skim `{}`",
            state.display()
        )));
        assert!(
            result.text.find("fl state render").unwrap() < result.text.find("STATE.md").unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn hydration_role_symlink_resolves_without_a_journalist_alias() {
        let f = Fixture::new();
        let source = f.workspace.join(".agents/roles/scribe.md");
        std::fs::write(&source, "scribe").unwrap();
        let role = f.workspace.join(".agents/roles/journalist.md");
        std::os::unix::fs::symlink("scribe.md", &role).unwrap();
        let result = f.prompt(&f.project, "journalist", RoleGuidance::File);
        assert!(result.warnings.is_empty());
        assert!(
            result
                .text
                .contains(&format!("Read your role file(s): `{}`", source.display()))
        );
        std::fs::remove_file(role).unwrap();
        let missing = f.prompt(&f.project, "journalist", RoleGuidance::File);
        assert!(
            missing
                .warnings
                .iter()
                .any(|w| w.contains("role file not found"))
        );
        assert!(!missing.text.contains(&source.display().to_string()));
    }

    #[test]
    fn hydration_catalog_text_is_unchanged_and_structural_custom_never_gets_builtin_file() {
        let f = Fixture::new();
        let authored = "Read `.agents/roles/assistant.md`.\nKeep this text exactly.";
        let result = f.prompt(&f.project, "council-chair", RoleGuidance::Catalog(authored));
        assert!(result.warnings.is_empty());
        assert!(result.text.contains(authored));
        assert!(result.text.contains(&format!(
            "Catalog guidance workspace-relative base: `{}`",
            f.workspace.display()
        )));
        assert!(!result.text.contains("council-chair.md"));
        let result = f.prompt(
            &f.project,
            "custom:assistant",
            RoleGuidance::StructuralCustom,
        );
        assert!(!result.text.contains("roles/assistant.md"));
        assert!(result.text.contains("grants no structural authority"));
    }

    #[test]
    fn hydration_missing_files_are_explicit_without_legacy_warning_or_read_instruction() {
        let f = Fixture::new();
        for file in [
            f.project.join("PERSONA.md"),
            f.workspace.join(".agents/AGENTS.md"),
            f.workspace.join(".agents/roles/assistant.md"),
        ] {
            std::fs::remove_file(file).unwrap();
        }
        let result = f.prompt(&f.project, "assistant", RoleGuidance::File);
        assert_eq!(result.warnings.len(), 3);
        assert!(
            result
                .text
                .contains("Missing guidance: project PERSONA.md not found")
        );
        assert!(
            result
                .text
                .contains("Missing guidance: workspace AGENTS.md not found")
        );
        assert!(
            result
                .text
                .contains("Missing guidance: role file not found")
        );
        assert!(!result.text.contains("STATE.md"));
        assert!(!result.text.contains(&format!(
            "Read `{}`",
            f.project.join("PERSONA.md").display()
        )));
    }

    #[test]
    fn hydration_no_known_root_never_guesses_parent_as_workspace() {
        let home = TempDir::new().unwrap();
        let child = home.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let context = Context::resolve(
            &MemStore::default(),
            Some(home.path()),
            None,
            Some(&child),
            "child",
        );
        assert!(context.workspace.is_none());
        assert!(context.project.is_none());
    }

    #[test]
    fn hydration_ui_roles_share_existing_file_without_changing_identity() {
        let f = Fixture::new();
        let role_file = f.workspace.join(".agents/roles/ui-developer.md");
        std::fs::write(&role_file, "UI").unwrap();
        for role in ["web-developer", "mobile-developer", "ui-developer"] {
            let result = f.prompt(&f.project, role, RoleGuidance::File);
            assert!(result.text.contains(&format!(
                "Read your role file(s): `{}`",
                role_file.display()
            )));
            assert!(result.text.contains(&format!("--role {role} --replace")));
        }
    }
}
