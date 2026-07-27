use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::gleam_toml::{GleamPathDependency, GomoDevConfig, GomoTargetConfig, parse_manifest};
use crate::task::{Task, TaskDefinition, TaskScope};

const CONFIG_FILE_NAME: &str = "gomo.toml";
const DEFAULT_CACHE_DIR: &str = ".gomo/cache";
const DEFAULT_CACHE_MAX_AGE_DAYS: u64 = 30;
const DEFAULT_CACHE_MAX_SIZE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const DEFAULT_PROJECT_GLOBS: &[&str] = &["apps/*", "libs/*", "services/*"];

/// Configured default concurrency for task-running commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultParallelism {
    Auto,
    Fixed(usize),
}

/// A discovered Gomo workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Canonical workspace root containing `gomo.toml`.
    pub root: PathBuf,
    /// Absolute local cache directory.
    pub cache_dir: PathBuf,
    /// Maximum age for local cache entries. `None` disables age pruning.
    pub cache_max_age_seconds: Option<u64>,
    /// Maximum local cache size. `None` disables size pruning.
    pub cache_max_size_bytes: Option<u64>,
    /// Configured project root globs, relative to the workspace root.
    pub project_globs: Vec<String>,
    /// Configured default concurrency for task-running commands.
    pub default_parallelism: DefaultParallelism,
    /// Per-target workspace-level inputs that affect every project.
    pub global_target_inputs: BTreeMap<String, Vec<String>>,
    /// Workspace policy for checking resolved dependency versions.
    pub dependency_versions: DependencyVersionConfig,
    /// Workspace-scoped and reusable root task definitions.
    pub tasks: BTreeMap<String, Task>,
    /// Gleam projects discovered under the configured roots or referenced by local path dependencies.
    pub projects: Vec<Project>,
}

/// A Gleam package discovered in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Gleam package name.
    pub name: String,
    /// Gleam package version, when declared.
    pub version: Option<String>,
    /// Gleam target, defaulting to `erlang` when omitted.
    pub target: String,
    /// Canonical project root directory.
    pub root: PathBuf,
    /// Project root path relative to the workspace root.
    pub root_relative_path: PathBuf,
    /// Absolute path to this project's `gleam.toml`.
    pub manifest_path: PathBuf,
    /// Local path dependencies declared by this project.
    pub path_dependencies: Vec<GleamPathDependency>,
    /// Per-target Gomo config declared by this project.
    pub gomo_targets: BTreeMap<String, GomoTargetConfig>,
    /// Development-process configuration declared in this project's manifest.
    pub gomo_dev: GomoDevConfig,
    /// Named tasks explicitly exposed by this project.
    pub gomo_tasks: BTreeMap<String, Task>,
    pub(crate) gomo_task_definitions: BTreeMap<String, TaskDefinition>,
}

/// Workspace policy for checking resolved dependency versions across `manifest.toml` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyVersionConfig {
    /// Whether `gomo doctor` should run the dependency version check.
    pub enabled: bool,
    /// Whether local path packages in lock manifests should be checked.
    pub include_local: bool,
    /// Dependency package names to skip.
    pub ignore: Vec<String>,
}

/// Discover the nearest workspace from a starting path.
pub fn discover_from(start: impl AsRef<Path>) -> Result<Workspace> {
    let root = find_workspace_root(start.as_ref())?;
    discover(root)
}

/// Discover projects under a known workspace root.
pub fn discover(root: impl AsRef<Path>) -> Result<Workspace> {
    let root = root.as_ref();
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace root {}", root.display()))?;
    let config = parse_workspace_config(&root)?;

    let mut projects = Vec::new();
    let project_roots = discover_project_roots(&root, &config.project_globs)?;

    for project_root in project_roots {
        projects.push(load_project(&root, project_root)?);
    }

    reject_duplicate_names(&projects)?;
    let tasks = resolve_root_tasks(config.tasks)?;
    resolve_project_tasks(&tasks, &mut projects)?;
    validate_task_references(&tasks, &projects)?;

    Ok(Workspace {
        root,
        cache_dir: config.cache_dir,
        cache_max_age_seconds: config.cache_max_age_seconds,
        cache_max_size_bytes: config.cache_max_size_bytes,
        project_globs: config.project_globs,
        default_parallelism: config.default_parallelism,
        global_target_inputs: config.global_target_inputs,
        dependency_versions: config.dependency_versions,
        tasks,
        projects,
    })
}

fn resolve_root_tasks(
    definitions: BTreeMap<String, TaskDefinition>,
) -> Result<BTreeMap<String, Task>> {
    let mut tasks = BTreeMap::new();
    for (name, definition) in definitions {
        if definition.extends.is_some() {
            bail!("root task `{name}` cannot use `extends`");
        }
        let task = definition.resolve(&name, TaskScope::Workspace, None)?;
        tasks.insert(name, task);
    }
    Ok(tasks)
}

fn resolve_project_tasks(
    root_tasks: &BTreeMap<String, Task>,
    projects: &mut [Project],
) -> Result<()> {
    for project in projects {
        let definitions = std::mem::take(&mut project.gomo_task_definitions);
        let mut exposed = BTreeMap::new();

        for (name, definition) in definitions {
            let base = if let Some(extends) = definition.extends.as_deref() {
                let task = root_tasks.get(extends).ok_or_else(|| {
                    anyhow::anyhow!(
                        "project `{}` task `{name}` extends unknown root task `{extends}`",
                        project.name
                    )
                })?;
                if task.scope != TaskScope::Project {
                    bail!(
                        "project `{}` task `{name}` cannot extend workspace task `{extends}`",
                        project.name
                    );
                }
                Some(task)
            } else {
                if root_tasks.contains_key(&name) {
                    bail!(
                        "project `{}` task `{name}` collides with a root task; add extends = \"{name}\" to expose it explicitly",
                        project.name
                    );
                }
                None
            };
            let task = definition.resolve(&name, TaskScope::Project, base)?;
            if task.scope != TaskScope::Project {
                bail!(
                    "project `{}` task `{name}` must have project scope",
                    project.name
                );
            }
            exposed.insert(name, task);
        }

        for (target, config) in &project.gomo_targets {
            let Some(task_name) = config.task.as_deref() else {
                continue;
            };
            if exposed.contains_key(task_name) {
                continue;
            }
            let root_task = root_tasks.get(task_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "project `{}` binds target `{target}` to unknown task `{task_name}`",
                    project.name
                )
            })?;
            if root_task.scope != TaskScope::Project {
                bail!(
                    "project `{}` binds target `{target}` to workspace task `{task_name}`; target bindings require project tasks",
                    project.name
                );
            }
            exposed.insert(task_name.to_string(), root_task.clone());
        }
        project.gomo_tasks = exposed;
    }
    Ok(())
}

fn validate_task_references(
    root_tasks: &BTreeMap<String, Task>,
    projects: &[Project],
) -> Result<()> {
    for task in root_tasks
        .values()
        .filter(|task| task.scope == TaskScope::Workspace)
    {
        for dependency in &task.depends_on {
            let referenced = root_tasks.get(dependency).ok_or_else(|| {
                anyhow::anyhow!(
                    "task `{}` depends on unknown task `{dependency}`",
                    task.name
                )
            })?;
            if referenced.scope != TaskScope::Workspace {
                bail!(
                    "workspace task `{}` cannot depend directly on project task `{dependency}`",
                    task.name
                );
            }
            if referenced.persistent {
                bail!(
                    "task `{}` cannot use persistent task `{dependency}` as a prerequisite",
                    task.name
                );
            }
        }
        validate_step_references(task, None, root_tasks, projects)?;
    }

    for project in projects {
        for task in project.gomo_tasks.values() {
            for dependency in &task.depends_on {
                let referenced = project.gomo_tasks.get(dependency).ok_or_else(|| {
                    anyhow::anyhow!(
                        "project `{}` task `{}` depends on task `{dependency}`, which that project does not expose",
                        project.name,
                        task.name
                    )
                })?;
                if referenced.persistent {
                    bail!(
                        "project `{}` task `{}` cannot use persistent task `{dependency}` as a prerequisite",
                        project.name,
                        task.name
                    );
                }
            }
            validate_step_references(task, Some(project), root_tasks, projects)?;
        }
    }
    reject_task_cycles(root_tasks, projects)?;
    Ok(())
}

fn validate_step_references(
    task: &Task,
    owner_project: Option<&Project>,
    root_tasks: &BTreeMap<String, Task>,
    projects: &[Project],
) -> Result<()> {
    for step in &task.steps {
        if let Some(action) = &step.task {
            if let Some(project_name) = action.project.as_deref() {
                let project = projects
                    .iter()
                    .find(|project| project.name == project_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "task `{}` references unknown project `{project_name}`",
                            task.name
                        )
                    })?;
                if !project.gomo_tasks.contains_key(&action.name) {
                    bail!(
                        "task `{}` invokes `{project_name}:{}`, but that project does not expose the task",
                        task.name,
                        action.name
                    );
                }
            } else if let Some(project) = owner_project {
                if !project.gomo_tasks.contains_key(&action.name) {
                    bail!(
                        "project `{}` task `{}` invokes task `{}`, which that project does not expose",
                        project.name,
                        task.name,
                        action.name
                    );
                }
            } else if !root_tasks
                .get(&action.name)
                .is_some_and(|referenced| referenced.scope == TaskScope::Workspace)
            {
                bail!(
                    "workspace task `{}` invokes unknown workspace task `{}`",
                    task.name,
                    action.name
                );
            }
        }
        let referenced_projects = step
            .target
            .as_ref()
            .and_then(|action| action.project.as_deref())
            .into_iter()
            .chain(step.dev.as_ref().map(|action| action.project.as_str()))
            .chain(
                step.watch
                    .as_ref()
                    .and_then(|action| action.project.as_deref()),
            )
            .chain(step.module.as_ref().map(|action| action.project.as_str()));
        for project_name in referenced_projects {
            if !projects.iter().any(|project| project.name == project_name) {
                bail!(
                    "task `{}` references unknown project `{project_name}`",
                    task.name
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TaskNode {
    project: Option<String>,
    task: String,
}

impl TaskNode {
    fn workspace(task: impl Into<String>) -> Self {
        Self {
            project: None,
            task: task.into(),
        }
    }

    fn project(project: impl Into<String>, task: impl Into<String>) -> Self {
        Self {
            project: Some(project.into()),
            task: task.into(),
        }
    }

    fn display(&self) -> String {
        match &self.project {
            Some(project) => format!("{project}:{}", self.task),
            None => self.task.clone(),
        }
    }
}

fn reject_task_cycles(root_tasks: &BTreeMap<String, Task>, projects: &[Project]) -> Result<()> {
    let mut edges: BTreeMap<TaskNode, Vec<TaskNode>> = BTreeMap::new();

    for task in root_tasks
        .values()
        .filter(|task| task.scope == TaskScope::Workspace)
    {
        let node = TaskNode::workspace(&task.name);
        edges.insert(node.clone(), task_reference_nodes(task, None));
    }

    for project in projects {
        for task in project.gomo_tasks.values() {
            let node = TaskNode::project(&project.name, &task.name);
            edges.insert(
                node.clone(),
                task_reference_nodes(task, Some(&project.name)),
            );
        }
    }

    fn visit(
        node: &TaskNode,
        edges: &BTreeMap<TaskNode, Vec<TaskNode>>,
        visiting: &mut Vec<TaskNode>,
        visited: &mut BTreeSet<TaskNode>,
    ) -> Result<()> {
        if let Some(index) = visiting.iter().position(|candidate| candidate == node) {
            let mut cycle = visiting[index..]
                .iter()
                .map(TaskNode::display)
                .collect::<Vec<_>>();
            cycle.push(node.display());
            bail!("task cycle: {}", cycle.join(" -> "));
        }
        if visited.contains(node) {
            return Ok(());
        }
        visiting.push(node.clone());
        if let Some(dependencies) = edges.get(node) {
            for dependency in dependencies {
                if edges.contains_key(dependency) {
                    visit(dependency, edges, visiting, visited)?;
                }
            }
        }
        visiting.pop();
        visited.insert(node.clone());
        Ok(())
    }

    let mut visited = BTreeSet::new();
    for node in edges.keys() {
        visit(node, &edges, &mut Vec::new(), &mut visited)?;
    }
    Ok(())
}

fn task_reference_nodes(task: &Task, owner_project: Option<&str>) -> Vec<TaskNode> {
    let mut nodes = Vec::new();
    for dependency in &task.depends_on {
        nodes.push(match owner_project {
            Some(project) => TaskNode::project(project, dependency),
            None => TaskNode::workspace(dependency),
        });
    }
    for step in &task.steps {
        let Some(action) = &step.task else {
            continue;
        };
        let project = action.project.as_deref().or(owner_project);
        nodes.push(match project {
            Some(project) => TaskNode::project(project, &action.name),
            None => TaskNode::workspace(&action.name),
        });
    }
    nodes
}

fn find_workspace_root(start: &Path) -> Result<PathBuf> {
    let mut current = start
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", start.display()))?;

    if current.is_file() {
        current.pop();
    }

    loop {
        if current.join(CONFIG_FILE_NAME).is_file() {
            return Ok(current);
        }

        if !current.pop() {
            bail!(
                "could not find `{}` from {} or any parent directory",
                CONFIG_FILE_NAME,
                start.display()
            );
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGomoConfig {
    #[serde(default)]
    workspace: RawWorkspaceConfig,
    #[serde(default)]
    cache: RawCacheConfig,
    #[serde(default)]
    dependency_versions: Option<RawDependencyVersionConfig>,
    #[serde(default)]
    tasks: BTreeMap<String, TaskDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspaceConfig {
    #[serde(default = "default_project_globs_vec")]
    project_roots: Vec<String>,
    #[serde(default = "default_parallelism_string")]
    default_parallelism: String,
    #[serde(default)]
    build: Option<RawWorkspaceTarget>,
    #[serde(default)]
    format: Option<RawWorkspaceTarget>,
    #[serde(default)]
    test: Option<RawWorkspaceTarget>,
}

impl Default for RawWorkspaceConfig {
    fn default() -> Self {
        Self {
            project_roots: default_project_globs_vec(),
            default_parallelism: default_parallelism_string(),
            build: None,
            format: None,
            test: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspaceTarget {
    inputs: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCacheConfig {
    dir: Option<String>,
    max_age_days: Option<u64>,
    max_size_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependencyVersionConfig {
    enabled: Option<bool>,
    include_local: Option<bool>,
    ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceConfig {
    cache_dir: PathBuf,
    cache_max_age_seconds: Option<u64>,
    cache_max_size_bytes: Option<u64>,
    project_globs: Vec<String>,
    default_parallelism: DefaultParallelism,
    global_target_inputs: BTreeMap<String, Vec<String>>,
    dependency_versions: DependencyVersionConfig,
    tasks: BTreeMap<String, TaskDefinition>,
}

fn parse_workspace_config(root: &Path) -> Result<WorkspaceConfig> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let raw_config = if config_path.is_file() {
        let text = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        toml::from_str::<RawGomoConfig>(&text)
            .with_context(|| format!("invalid TOML in {}", config_path.display()))?
    } else {
        RawGomoConfig::default()
    };

    let cache_dir = raw_config
        .cache
        .dir
        .unwrap_or_else(|| DEFAULT_CACHE_DIR.to_string());
    let cache_dir = PathBuf::from(cache_dir);
    let cache_dir = if cache_dir.is_absolute() {
        cache_dir
    } else {
        root.join(cache_dir)
    };
    let cache_max_age_seconds = raw_config
        .cache
        .max_age_days
        .unwrap_or(DEFAULT_CACHE_MAX_AGE_DAYS)
        .checked_mul(24 * 60 * 60)
        .filter(|seconds| *seconds > 0);
    let cache_max_size_bytes = Some(
        raw_config
            .cache
            .max_size_bytes
            .unwrap_or(DEFAULT_CACHE_MAX_SIZE_BYTES),
    )
    .filter(|bytes| *bytes > 0);

    let RawWorkspaceConfig {
        project_roots,
        default_parallelism,
        build,
        format,
        test,
    } = raw_config.workspace;
    let project_globs = normalize_project_globs(project_roots)?;
    let default_parallelism = parse_default_parallelism(&default_parallelism)?;

    Ok(WorkspaceConfig {
        cache_dir,
        cache_max_age_seconds,
        cache_max_size_bytes,
        project_globs,
        default_parallelism,
        global_target_inputs: collect_workspace_target_inputs(build, format, test),
        dependency_versions: normalize_dependency_version_config(raw_config.dependency_versions)?,
        tasks: raw_config.tasks,
    })
}

fn normalize_dependency_version_config(
    config: Option<RawDependencyVersionConfig>,
) -> Result<DependencyVersionConfig> {
    let Some(config) = config else {
        return Ok(DependencyVersionConfig {
            enabled: false,
            include_local: true,
            ignore: Vec::new(),
        });
    };

    Ok(DependencyVersionConfig {
        enabled: config.enabled.unwrap_or(true),
        include_local: config.include_local.unwrap_or(true),
        ignore: normalize_dependency_version_ignore(config.ignore.unwrap_or_default())?,
    })
}

fn normalize_dependency_version_ignore(ignore: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for dependency in ignore {
        let dependency = dependency.trim().to_string();
        if dependency.is_empty() {
            bail!("dependency_versions.ignore entries must not be empty");
        }
        if seen.insert(dependency.clone()) {
            normalized.push(dependency);
        }
    }

    Ok(normalized)
}

fn default_project_globs_vec() -> Vec<String> {
    DEFAULT_PROJECT_GLOBS
        .iter()
        .map(|project_glob| (*project_glob).to_string())
        .collect()
}

fn default_parallelism_string() -> String {
    "auto".to_string()
}

fn parse_default_parallelism(value: &str) -> Result<DefaultParallelism> {
    let value = value.trim();
    if value == "auto" {
        return Ok(DefaultParallelism::Auto);
    }

    let parallelism = value.parse::<usize>().with_context(|| {
        format!("workspace.default_parallelism must be `auto` or a positive integer, got `{value}`")
    })?;
    if parallelism == 0 {
        bail!("workspace.default_parallelism must be greater than zero");
    }

    Ok(DefaultParallelism::Fixed(parallelism))
}

fn normalize_project_globs(project_globs: Vec<String>) -> Result<Vec<String>> {
    if project_globs.is_empty() {
        bail!("workspace.project_roots must contain at least one project root glob");
    }

    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();

    for project_glob in project_globs {
        let project_glob = project_glob.trim().to_string();
        validate_project_root_glob(&project_glob)?;
        if seen.insert(project_glob.clone()) {
            normalized.push(project_glob);
        }
    }

    Ok(normalized)
}

fn collect_workspace_target_inputs(
    build: Option<RawWorkspaceTarget>,
    format: Option<RawWorkspaceTarget>,
    test: Option<RawWorkspaceTarget>,
) -> BTreeMap<String, Vec<String>> {
    let mut targets = BTreeMap::new();
    insert_workspace_target_inputs(&mut targets, "build", build);
    insert_workspace_target_inputs(&mut targets, "format", format);
    insert_workspace_target_inputs(&mut targets, "test", test);
    targets
}

fn insert_workspace_target_inputs(
    targets: &mut BTreeMap<String, Vec<String>>,
    target: &str,
    config: Option<RawWorkspaceTarget>,
) {
    if let Some(inputs) = config.and_then(|config| config.inputs) {
        targets.insert(target.to_string(), inputs);
    }
}

fn validate_project_root_glob(project_glob: &str) -> Result<()> {
    if project_glob.trim().is_empty() {
        bail!("workspace.project_roots entries must not be empty");
    }

    let path = Path::new(project_glob);
    if path.is_absolute() {
        bail!(
            "workspace.project_roots entry `{project_glob}` must be relative to the workspace root"
        );
    }

    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            bail!(
                "workspace.project_roots entry `{project_glob}` must not leave the workspace root"
            );
        }
    }

    let wildcard_count = project_glob.matches('*').count();
    if wildcard_count > 1 || (wildcard_count == 1 && !project_glob.ends_with("/*")) {
        bail!(
            "workspace.project_roots entry `{project_glob}` uses an unsupported glob; only exact paths and direct-child globs like `apps/*` are supported"
        );
    }

    Ok(())
}

fn candidate_project_roots(root: &Path, project_globs: &[String]) -> Result<Vec<PathBuf>> {
    let mut project_roots = BTreeSet::new();

    for project_glob in project_globs {
        for project_root in expand_project_root_glob(root, project_glob)? {
            if project_root.join("gleam.toml").is_file() {
                project_roots.insert(project_root);
            }
        }
    }

    Ok(project_roots.into_iter().collect())
}

fn discover_project_roots(root: &Path, project_globs: &[String]) -> Result<Vec<PathBuf>> {
    let mut project_roots = BTreeSet::new();
    let mut pending = Vec::new();

    for project_root in candidate_project_roots(root, project_globs)? {
        insert_project_root(&mut project_roots, &mut pending, project_root)?;
    }

    while let Some(project_root) = pending.pop() {
        let manifest = parse_manifest(&project_root.join("gleam.toml"))?;
        for dependency in &manifest.path_dependencies {
            let Some(dependency_root) =
                discoverable_path_dependency_root(root, &project_root, dependency)
            else {
                continue;
            };
            insert_project_root(&mut project_roots, &mut pending, dependency_root)?;
        }
    }

    Ok(project_roots.into_iter().collect())
}

fn insert_project_root(
    project_roots: &mut BTreeSet<PathBuf>,
    pending: &mut Vec<PathBuf>,
    project_root: PathBuf,
) -> Result<()> {
    let project_root = project_root
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", project_root.display()))?;
    if project_roots.insert(project_root.clone()) {
        pending.push(project_root);
    }

    Ok(())
}

fn discoverable_path_dependency_root(
    workspace_root: &Path,
    project_root: &Path,
    dependency: &GleamPathDependency,
) -> Option<PathBuf> {
    let dependency_root = project_root.join(&dependency.path).canonicalize().ok()?;
    if !dependency_root.starts_with(workspace_root) || !dependency_root.join("gleam.toml").is_file()
    {
        return None;
    }

    Some(dependency_root)
}

fn load_project(root: &Path, project_root: PathBuf) -> Result<Project> {
    let manifest_path = project_root.join("gleam.toml");
    let manifest = parse_manifest(&manifest_path)?;
    let root_relative_path = project_root
        .strip_prefix(root)
        .unwrap_or(project_root.as_path())
        .to_path_buf();

    Ok(Project {
        name: manifest.name,
        version: manifest.version,
        target: manifest.target,
        root: project_root,
        root_relative_path,
        manifest_path,
        path_dependencies: manifest.path_dependencies,
        gomo_targets: manifest.gomo_targets,
        gomo_dev: manifest.gomo_dev,
        gomo_tasks: BTreeMap::new(),
        gomo_task_definitions: manifest.gomo_tasks,
    })
}

fn expand_project_root_glob(root: &Path, project_glob: &str) -> Result<Vec<PathBuf>> {
    validate_project_root_glob(project_glob)?;

    if let Some(parent_glob) = project_glob.strip_suffix("/*") {
        let parent_dir = root.join(parent_glob);
        if !parent_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&parent_dir)
            .with_context(|| format!("failed to read {}", parent_dir.display()))?;
        let mut project_roots = Vec::new();
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read entry under {}", parent_dir.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if file_type.is_dir() {
                project_roots.push(entry.path());
            }
        }
        return Ok(project_roots);
    }

    let project_root = root.join(project_glob);
    if project_root.is_dir() {
        Ok(vec![project_root])
    } else {
        Ok(Vec::new())
    }
}

fn reject_duplicate_names(projects: &[Project]) -> Result<()> {
    let mut seen = HashMap::<&str, &Project>::new();

    for project in projects {
        if let Some(previous) = seen.insert(project.name.as_str(), project) {
            bail!(
                "duplicate Gleam package name `{}` found in {} and {}",
                project.name,
                previous.manifest_path.display(),
                project.manifest_path.display()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestWorkspace;

    #[test]
    fn discovers_projects_from_default_roots() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
target = "javascript"

[dependencies]
shared = { path = "../../libs/shared" }
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(workspace.projects[0].name, "demo");
        assert_eq!(workspace.projects[0].version.as_deref(), Some("0.1.0"));
        assert_eq!(workspace.projects[0].target, "javascript");
        assert_eq!(
            workspace.projects[0].root_relative_path,
            PathBuf::from("apps/demo")
        );
        assert_eq!(workspace.projects[0].path_dependencies[0].name, "shared");
        assert_eq!(workspace.projects[1].name, "shared");
        assert_eq!(workspace.projects[1].target, "erlang");
        assert_eq!(workspace.project_globs, default_project_globs_vec());
        assert_eq!(workspace.default_parallelism, DefaultParallelism::Auto);
        assert!(!workspace.dependency_versions.enabled);
        assert!(workspace.dependency_versions.include_local);
        assert!(workspace.dependency_versions.ignore.is_empty());
    }

    #[test]
    fn discovers_projects_from_configured_roots() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["tools/*"]
"#,
        );
        test_workspace.write_manifest(
            "tools/esgleam",
            r#"
name = "esgleam"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.project_globs, vec!["tools/*".to_string()]);
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.projects[0].name, "esgleam");
        assert_eq!(
            workspace.projects[0].root_relative_path,
            PathBuf::from("tools/esgleam")
        );
    }

    #[test]
    fn discovers_regular_path_dependencies_outside_configured_roots() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
esgleam = { path = "../../tools/esgleam" }
"#,
        );
        test_workspace.write_manifest(
            "tools/esgleam",
            r#"
name = "esgleam"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.projects.len(), 2);
        assert!(
            workspace
                .projects
                .iter()
                .any(|project| project.name == "demo")
        );
        assert!(workspace.projects.iter().any(|project| {
            project.name == "esgleam"
                && project.root_relative_path == PathBuf::from("tools/esgleam")
        }));
    }

    #[test]
    fn discovers_workspace_from_nested_directory() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        let nested_dir = test_workspace.path().join("apps/demo/src");
        fs::create_dir_all(&nested_dir).expect("nested dir should be created");

        let workspace = discover_from(&nested_dir).expect("workspace should be discovered");

        assert_eq!(
            workspace.root,
            test_workspace.path().canonicalize().unwrap()
        );
        assert_eq!(workspace.projects[0].name, "demo");
    }

    #[test]
    fn parses_cache_dir_from_gomo_config() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[cache]
dir = "tmp/gomo-cache"
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.cache_dir, workspace.root.join("tmp/gomo-cache"));
    }

    #[test]
    fn parses_cache_pruning_from_gomo_config() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[cache]
max_age_days = 7
max_size_bytes = 1024
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.cache_max_age_seconds, Some(7 * 24 * 60 * 60));
        assert_eq!(workspace.cache_max_size_bytes, Some(1024));
    }

    #[test]
    fn parses_workspace_target_inputs_from_gomo_config() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[workspace.test]
inputs = ["gomo.toml", ".github/workflows/**"]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(
            workspace.global_target_inputs.get("test"),
            Some(&vec![
                "gomo.toml".to_string(),
                ".github/workflows/**".to_string(),
            ])
        );
    }

    #[test]
    fn parses_default_parallelism_from_gomo_config() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
default_parallelism = "4"
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert_eq!(workspace.default_parallelism, DefaultParallelism::Fixed(4));
    }

    #[test]
    fn parses_dependency_version_config_from_gomo_config() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[dependency_versions]
enabled = true
include_local = false
ignore = ["gleam_stdlib", "gleam_stdlib", " lustre "]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert!(workspace.dependency_versions.enabled);
        assert!(!workspace.dependency_versions.include_local);
        assert_eq!(
            workspace.dependency_versions.ignore,
            vec!["gleam_stdlib".to_string(), "lustre".to_string()]
        );
    }

    #[test]
    fn dependency_version_config_table_defaults_to_enabled() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[dependency_versions]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = discover(test_workspace.path()).expect("workspace should be discovered");

        assert!(workspace.dependency_versions.enabled);
        assert!(workspace.dependency_versions.include_local);
        assert!(workspace.dependency_versions.ignore.is_empty());
    }

    #[test]
    fn rejects_empty_dependency_version_ignore_entries() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[dependency_versions]
ignore = [""]
"#,
        );

        let error = discover(test_workspace.path()).expect_err("empty ignore should fail");

        assert!(
            error
                .to_string()
                .contains("dependency_versions.ignore entries must not be empty")
        );
    }

    #[test]
    fn rejects_unknown_gomo_config_fields() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
project_rootz = ["tools/*"]
"#,
        );

        let error = discover(test_workspace.path()).expect_err("unknown config should fail");
        let error_chain = format!("{error:#}");

        assert!(error.to_string().contains("invalid TOML"));
        assert!(error_chain.contains("unknown field"));
        assert!(error_chain.contains("project_rootz"));
    }

    #[test]
    fn rejects_invalid_default_parallelism() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
default_parallelism = "0"
"#,
        );

        let error = discover(test_workspace.path()).expect_err("invalid parallelism should fail");

        assert!(
            error
                .to_string()
                .contains("workspace.default_parallelism must be greater than zero")
        );
    }

    #[test]
    fn requires_gomo_config_for_upward_discovery() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");

        let error = discover_from(test_workspace.path()).expect_err("missing config should fail");

        assert!(error.to_string().contains("could not find `gomo.toml`"));
    }

    #[test]
    fn rejects_duplicate_project_names() {
        let test_workspace = TestWorkspace::new("gomo-workspace-test");
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "duplicate"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "libs/two",
            r#"
name = "duplicate"
version = "0.1.0"
"#,
        );

        let error = discover(test_workspace.path()).expect_err("duplicates should fail");

        assert!(
            error
                .to_string()
                .contains("duplicate Gleam package name `duplicate`")
        );
    }

    #[test]
    fn rejects_cross_project_task_cycles() {
        let test_workspace = TestWorkspace::new("gomo-task-cycle");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.ping]
scope = "project"
steps = [{ task = { name = "pong", project = "two" } }]

[tasks.pong]
scope = "project"
steps = [{ task = { name = "ping", project = "one" } }]
"#,
        );
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "one"
version = "0.1.0"

[tools.gomo.tasks.ping]
extends = "ping"
"#,
        );
        test_workspace.write_manifest(
            "apps/two",
            r#"
name = "two"
version = "0.1.0"

[tools.gomo.tasks.pong]
extends = "pong"
"#,
        );

        let error = discover(test_workspace.path()).expect_err("cross-project cycles should fail");
        assert!(error.to_string().contains("task cycle:"));
    }
}
