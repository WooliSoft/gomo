use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::graph::ProjectGraph;
use crate::runner::{CommandOptions, Target, TaskExecution};
use crate::workspace::{Project, Workspace};

pub(crate) const CACHE_SCHEMA_VERSION: &str = "v4";

const CACHE_STORE_LOCK_ATTEMPTS: usize = 300;
const CACHE_STORE_LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
const STALE_CACHE_WORK_DIR_SECONDS: u64 = 24 * 60 * 60;
const RESTORE_WORK_DIR_PREFIX: &str = ".gomo-restore-";

const DEFAULT_BUILD_INPUTS: &[&str] = &[
    "gleam.toml",
    "manifest.toml",
    "src/**",
    "package.json",
    "bun.lock",
    "vite.config.*",
    "tsconfig*.json",
    "index.html",
];
const DEFAULT_TEST_INPUTS: &[&str] = &[
    "gleam.toml",
    "manifest.toml",
    "src/**",
    "test/**",
    "package.json",
    "bun.lock",
    "vite.config.*",
    "tsconfig*.json",
    "index.html",
];
const DEFAULT_FORMAT_INPUTS: &[&str] = &["gleam.toml", "manifest.toml", "src/**", "test/**"];
const ENV_ALLOWLIST: &[&str] = &["GLEAM_ENV", "GLEAM_TARGET"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskHash {
    pub(crate) project: String,
    pub(crate) project_root: String,
    pub(crate) project_target: String,
    pub(crate) target: Target,
    pub(crate) command: String,
    pub(crate) hash: String,
    pub(crate) schema_version: String,
    pub(crate) gomo_version: String,
    pub(crate) gleam_version: String,
    pub(crate) operating_system: String,
    pub(crate) architecture: String,
    pub(crate) input_source: InputGlobSource,
    pub(crate) input_globs: Vec<String>,
    pub(crate) workspace_input_globs: Vec<String>,
    pub(crate) cached_folders: Vec<String>,
    pub(crate) manifest_hash: String,
    pub(crate) input_files: Vec<HashedInputFile>,
    pub(crate) workspace_input_files: Vec<HashedInputFile>,
    pub(crate) dependency_hashes: Vec<DependencyTaskHash>,
    pub(crate) environment: Vec<EnvironmentInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputGlobSource {
    BuiltIn,
    ProjectOverride,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashedInputFile {
    pub(crate) relative_path: String,
    pub(crate) content_hash: String,
    pub(crate) byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DependencyTaskHash {
    pub(crate) project: String,
    pub(crate) target: Target,
    pub(crate) hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvironmentInput {
    pub(crate) name: String,
    pub(crate) value: String,
}

/// Compute a cache key for a task using the installed Gleam binary version.
pub(crate) fn compute_task_hash(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project: &Project,
    target: Target,
) -> Result<TaskHash> {
    let gleam_version = gleam_version()?;
    compute_task_hash_with_gleam_version(workspace, graph, project, target, &gleam_version)
}

pub(crate) fn compute_task_hash_with_gleam_version(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project: &Project,
    target: Target,
    gleam_version: &str,
) -> Result<TaskHash> {
    let project_index = workspace
        .projects
        .iter()
        .map(|project| (project.name.as_str(), project))
        .collect::<BTreeMap<_, _>>();
    let mut memo = BTreeMap::new();

    compute_task_hash_inner(
        workspace,
        graph,
        &project_index,
        project.name.as_str(),
        target,
        gleam_version,
        &mut memo,
    )
}

pub(crate) fn gleam_version() -> Result<String> {
    let output = Command::new("gleam")
        .arg("--version")
        .output()
        .context("failed to run `gleam --version`")?;

    if !output.status.success() {
        bail!(
            "`gleam --version` failed with exit code {}",
            output.status.code().unwrap_or(1)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn compute_task_hash_inner(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project_index: &BTreeMap<&str, &Project>,
    project_name: &str,
    target: Target,
    gleam_version: &str,
    memo: &mut BTreeMap<(String, Target), TaskHash>,
) -> Result<TaskHash> {
    ensure_cacheable_target(target)?;

    let memo_key = (project_name.to_string(), target);
    if let Some(task_hash) = memo.get(&memo_key) {
        return Ok(task_hash.clone());
    }

    let project = project_index
        .get(project_name)
        .with_context(|| format!("unknown project `{project_name}`"))?;
    let input_config = effective_input_globs(project, target)?;
    let workspace_input_globs = workspace_input_globs(workspace, target);
    let cached_folders = effective_cached_folders(workspace, project, target)?;
    let cached_output_dirs = project_cached_output_dirs(workspace, project)?;
    let input_files = expand_input_globs(project, &input_config.globs, &cached_output_dirs)?;
    let workspace_input_files = expand_workspace_input_globs(workspace, &workspace_input_globs)?;
    let manifest_hash = hash_file(&project.manifest_path)?.content_hash;
    let environment = collect_environment();
    let operating_system = env::consts::OS;
    let architecture = env::consts::ARCH;
    let mut dependency_hashes = Vec::new();

    if let Some(dependency_target) = dependency_task_target(target) {
        for dependency in task_hash_dependencies(graph, project, target) {
            let dependency_hash = compute_task_hash_inner(
                workspace,
                graph,
                project_index,
                dependency.as_str(),
                dependency_target,
                gleam_version,
                memo,
            )?;
            dependency_hashes.push(DependencyTaskHash {
                project: dependency,
                target: dependency_target,
                hash: dependency_hash.hash,
            });
        }
    }

    let command = project_command(project, target)?;
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, "schema_version", CACHE_SCHEMA_VERSION);
    hash_field(&mut hasher, "gomo_version", env!("CARGO_PKG_VERSION"));
    hash_field(&mut hasher, "target", target.as_str());
    hash_field(&mut hasher, "command", command.as_str());
    hash_field(&mut hasher, "project_name", project.name.as_str());
    hash_field(
        &mut hasher,
        "project_root",
        normalize_path(&project.root_relative_path).as_str(),
    );
    hash_field(&mut hasher, "project_target", project.target.as_str());
    hash_field(&mut hasher, "manifest_hash", manifest_hash.as_str());
    hash_field(&mut hasher, "gleam_version", gleam_version);
    hash_field(&mut hasher, "operating_system", operating_system);
    hash_field(&mut hasher, "architecture", architecture);
    hash_field(
        &mut hasher,
        "input_glob_source",
        input_config.source.as_str(),
    );

    for input_glob in &input_config.globs {
        hash_field(&mut hasher, "input_glob", input_glob);
    }
    for input_glob in &workspace_input_globs {
        hash_field(&mut hasher, "workspace_input_glob", input_glob);
    }
    for cached_folder in &cached_folders {
        hash_field(&mut hasher, "cached_folder", cached_folder);
    }
    for input_file in &input_files {
        hash_field(&mut hasher, "input_path", input_file.relative_path.as_str());
        hash_field(&mut hasher, "input_hash", input_file.content_hash.as_str());
        hash_field(
            &mut hasher,
            "input_bytes",
            input_file.byte_len.to_string().as_str(),
        );
    }
    for input_file in &workspace_input_files {
        hash_field(
            &mut hasher,
            "workspace_input_path",
            input_file.relative_path.as_str(),
        );
        hash_field(
            &mut hasher,
            "workspace_input_hash",
            input_file.content_hash.as_str(),
        );
        hash_field(
            &mut hasher,
            "workspace_input_bytes",
            input_file.byte_len.to_string().as_str(),
        );
    }
    for dependency_hash in &dependency_hashes {
        hash_field(
            &mut hasher,
            "dependency_project",
            dependency_hash.project.as_str(),
        );
        hash_field(
            &mut hasher,
            "dependency_target",
            dependency_hash.target.as_str(),
        );
        hash_field(
            &mut hasher,
            "dependency_hash",
            dependency_hash.hash.as_str(),
        );
    }
    for environment_input in &environment {
        hash_field(&mut hasher, "env_name", environment_input.name.as_str());
        hash_field(&mut hasher, "env_value", environment_input.value.as_str());
    }

    let task_hash = TaskHash {
        project: project.name.clone(),
        project_root: normalize_path(&project.root_relative_path),
        project_target: project.target.clone(),
        target,
        command,
        hash: hasher.finalize().to_hex().to_string(),
        schema_version: CACHE_SCHEMA_VERSION.to_string(),
        gomo_version: env!("CARGO_PKG_VERSION").to_string(),
        gleam_version: gleam_version.to_string(),
        operating_system: operating_system.to_string(),
        architecture: architecture.to_string(),
        input_source: input_config.source,
        input_globs: input_config.globs,
        workspace_input_globs,
        cached_folders,
        manifest_hash,
        input_files,
        workspace_input_files,
        dependency_hashes,
        environment,
    };

    memo.insert(memo_key, task_hash.clone());
    Ok(task_hash)
}

fn ensure_cacheable_target(target: Target) -> Result<()> {
    if target.supports_cache() {
        return Ok(());
    }

    bail!("target `{target}` does not support cache keys")
}

fn dependency_task_target(target: Target) -> Option<Target> {
    match target {
        Target::Build | Target::Test => Some(Target::Build),
        Target::Format => None,
        Target::Clean => None,
    }
}

fn task_hash_dependencies(graph: &ProjectGraph, project: &Project, target: Target) -> Vec<String> {
    match target {
        Target::Test => graph.upstream_with_dev_for(project.name.as_str()),
        Target::Build if uses_custom_build_command(project) => {
            graph.upstream_with_dev_for(project.name.as_str())
        }
        Target::Build => graph.upstream_for(project.name.as_str()).to_vec(),
        Target::Format | Target::Clean => Vec::new(),
    }
}

fn uses_custom_build_command(project: &Project) -> bool {
    project
        .gomo_targets
        .get(Target::Build.as_str())
        .and_then(|config| config.command.as_ref())
        .is_some()
}

fn project_command(project: &Project, target: Target) -> Result<String> {
    CommandOptions::default().command_display(project, target)
}

fn effective_cached_folders(
    workspace: &Workspace,
    project: &Project,
    target: Target,
) -> Result<Vec<String>> {
    if target != Target::Build {
        return Ok(Vec::new());
    }

    let folders = project
        .gomo_targets
        .get(target.as_str())
        .and_then(|config| config.cached_folders.clone())
        .unwrap_or_else(|| vec!["build".to_string()]);
    for folder in &folders {
        let output_dir = project.root.join(folder);
        if output_dir.starts_with(&workspace.cache_dir)
            || workspace.cache_dir.starts_with(&output_dir)
        {
            bail!(
                "cached folder {} overlaps Gomo cache directory {}",
                output_dir.display(),
                workspace.cache_dir.display()
            );
        }
    }
    Ok(folders)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputGlobConfig {
    source: InputGlobSource,
    globs: Vec<String>,
}

impl InputGlobSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in defaults",
            Self::ProjectOverride => "project override",
        }
    }
}

pub(crate) fn target_inputs_match(
    project: &Project,
    target: Target,
    relative_path: &Path,
) -> Result<bool> {
    let input_config = effective_input_globs(project, target)?;
    let glob_set = input_glob_set(project, &input_config.globs)?;

    Ok(glob_set.is_match(relative_path))
}

pub(crate) fn workspace_inputs_match(
    workspace: &Workspace,
    target: Target,
    relative_path: &Path,
) -> Result<bool> {
    let input_globs = workspace_input_globs(workspace, target);
    if input_globs.is_empty() {
        return Ok(false);
    }

    let glob_set = workspace_input_glob_set(&input_globs)?;
    Ok(glob_set.is_match(relative_path))
}

fn effective_input_globs(project: &Project, target: Target) -> Result<InputGlobConfig> {
    let override_key = target.as_str();
    let (source, globs) = if let Some(input_overrides) = project
        .gomo_targets
        .get(override_key)
        .and_then(|config| config.inputs.clone())
    {
        (InputGlobSource::ProjectOverride, input_overrides)
    } else {
        (InputGlobSource::BuiltIn, default_input_globs(target)?)
    };

    if globs.is_empty() {
        bail!(
            "target `{target}` for project `{}` has no input globs",
            project.name
        );
    }

    Ok(InputGlobConfig {
        source,
        globs: dedupe_preserving_order(globs),
    })
}

fn workspace_input_globs(workspace: &Workspace, target: Target) -> Vec<String> {
    workspace
        .global_target_inputs
        .get(target.as_str())
        .cloned()
        .map(dedupe_preserving_order)
        .unwrap_or_default()
}

fn default_input_globs(target: Target) -> Result<Vec<String>> {
    let inputs = match target {
        Target::Build => DEFAULT_BUILD_INPUTS,
        Target::Format => DEFAULT_FORMAT_INPUTS,
        Target::Test => DEFAULT_TEST_INPUTS,
        Target::Clean => bail!("target `{target}` does not support cache keys"),
    };

    Ok(inputs.iter().map(|input| (*input).to_string()).collect())
}

fn dedupe_preserving_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();

    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }

    deduped
}

fn expand_input_globs(
    project: &Project,
    input_globs: &[String],
    cached_output_dirs: &BTreeSet<PathBuf>,
) -> Result<Vec<HashedInputFile>> {
    let glob_set = input_glob_set(project, input_globs)?;
    let mut matched_paths = BTreeSet::<PathBuf>::new();

    for entry in WalkDir::new(&project.root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored_input_dir(entry, cached_output_dirs))
    {
        let entry = entry.with_context(|| format!("failed to walk {}", project.root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative_path = entry.path().strip_prefix(&project.root).with_context(|| {
            format!(
                "failed to compute relative path for {} from {}",
                entry.path().display(),
                project.root.display()
            )
        })?;

        if glob_set.is_match(relative_path) {
            matched_paths.insert(relative_path.to_path_buf());
        }
    }

    matched_paths
        .into_iter()
        .map(|relative_path| hash_project_file(project, relative_path))
        .collect()
}

fn expand_workspace_input_globs(
    workspace: &Workspace,
    input_globs: &[String],
) -> Result<Vec<HashedInputFile>> {
    if input_globs.is_empty() {
        return Ok(Vec::new());
    }

    let glob_set = workspace_input_glob_set(input_globs)?;
    let cached_output_dirs = workspace_cached_output_dirs(workspace)?;
    let mut matched_paths = BTreeSet::<PathBuf>::new();

    for entry in WalkDir::new(&workspace.root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored_input_dir(entry, &cached_output_dirs))
    {
        let entry =
            entry.with_context(|| format!("failed to walk {}", workspace.root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative_path = entry
            .path()
            .strip_prefix(&workspace.root)
            .with_context(|| {
                format!(
                    "failed to compute relative path for {} from {}",
                    entry.path().display(),
                    workspace.root.display()
                )
            })?;

        if glob_set.is_match(relative_path) {
            matched_paths.insert(relative_path.to_path_buf());
        }
    }

    matched_paths
        .into_iter()
        .map(|relative_path| hash_workspace_file(workspace, relative_path))
        .collect()
}

fn project_cached_output_dirs(
    workspace: &Workspace,
    project: &Project,
) -> Result<BTreeSet<PathBuf>> {
    Ok(effective_cached_folders(workspace, project, Target::Build)?
        .iter()
        .map(|folder| project.root.join(folder))
        .collect())
}

fn workspace_cached_output_dirs(workspace: &Workspace) -> Result<BTreeSet<PathBuf>> {
    let mut output_dirs = BTreeSet::new();
    for project in &workspace.projects {
        output_dirs.extend(project_cached_output_dirs(workspace, project)?);
    }
    Ok(output_dirs)
}

fn is_ignored_input_dir(entry: &walkdir::DirEntry, cached_output_dirs: &BTreeSet<PathBuf>) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && (cached_output_dirs.contains(entry.path())
            || entry
                .file_name()
                .to_string_lossy()
                .starts_with(RESTORE_WORK_DIR_PREFIX))
}

fn input_glob_set(project: &Project, input_globs: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for input_glob in input_globs {
        validate_input_glob(input_glob).with_context(|| {
            format!(
                "invalid input glob `{input_glob}` for project `{}`",
                project.name
            )
        })?;
        builder.add(Glob::new(input_glob).with_context(|| {
            format!(
                "invalid input glob `{input_glob}` for project `{}`",
                project.name
            )
        })?);
    }

    builder.build().context("failed to build input glob set")
}

fn workspace_input_glob_set(input_globs: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for input_glob in input_globs {
        validate_workspace_input_glob(input_glob)
            .with_context(|| format!("invalid workspace input glob `{input_glob}`"))?;
        builder.add(
            Glob::new(input_glob)
                .with_context(|| format!("invalid workspace input glob `{input_glob}`"))?,
        );
    }

    builder
        .build()
        .context("failed to build workspace input glob set")
}

fn validate_input_glob(input_glob: &str) -> Result<()> {
    if input_glob.trim().is_empty() {
        bail!("input glob must not be empty");
    }

    let path = Path::new(input_glob);
    if path.is_absolute() {
        bail!("input glob must be relative to the project root");
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!("input glob must not leave the project root");
        }
    }

    Ok(())
}

fn validate_workspace_input_glob(input_glob: &str) -> Result<()> {
    if input_glob.trim().is_empty() {
        bail!("workspace input glob must not be empty");
    }

    let path = Path::new(input_glob);
    if path.is_absolute() {
        bail!("workspace input glob must be relative to the workspace root");
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!("workspace input glob must not leave the workspace root");
        }
    }

    Ok(())
}

fn hash_project_file(project: &Project, relative_path: PathBuf) -> Result<HashedInputFile> {
    let path = project.root.join(&relative_path);
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve input file {}", path.display()))?;
    if !canonical_path.starts_with(&project.root) {
        bail!(
            "input file {} for project `{}` resolves outside the project root",
            path.display(),
            project.name
        );
    }

    let hashed_file = hash_file(&path)?;
    Ok(HashedInputFile {
        relative_path: normalize_path(&relative_path),
        content_hash: hashed_file.content_hash,
        byte_len: hashed_file.byte_len,
    })
}

fn hash_workspace_file(workspace: &Workspace, relative_path: PathBuf) -> Result<HashedInputFile> {
    let path = workspace.root.join(&relative_path);
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace input file {}", path.display()))?;
    if !canonical_path.starts_with(&workspace.root) {
        bail!(
            "workspace input file {} resolves outside the workspace root",
            path.display()
        );
    }

    let hashed_file = hash_file(&path)?;
    Ok(HashedInputFile {
        relative_path: normalize_path(&relative_path),
        content_hash: hashed_file.content_hash,
        byte_len: hashed_file.byte_len,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileHash {
    content_hash: String,
    byte_len: u64,
}

fn hash_file(path: &Path) -> Result<FileHash> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read input file {}", path.display()))?;
    Ok(FileHash {
        content_hash: blake3::hash(&bytes).to_hex().to_string(),
        byte_len: bytes.len() as u64,
    })
}

fn collect_environment() -> Vec<EnvironmentInput> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|name| {
            env::var(name).ok().map(|value| EnvironmentInput {
                name: (*name).to_string(),
                value,
            })
        })
        .collect()
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_field(hasher: &mut blake3::Hasher, name: &str, value: &str) {
    hash_bytes(hasher, name.as_bytes());
    hash_bytes(hasher, value.as_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedTaskExecution {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheReset {
    pub(crate) cache_dir: PathBuf,
    pub(crate) removed: bool,
}

pub(crate) fn reset_cache(workspace: &Workspace) -> Result<CacheReset> {
    validate_cache_reset_path(workspace)?;

    let removed = match fs::symlink_metadata(&workspace.cache_dir) {
        Ok(_) => {
            remove_path(&workspace.cache_dir)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", workspace.cache_dir.display()));
        }
    };

    Ok(CacheReset {
        cache_dir: workspace.cache_dir.clone(),
        removed,
    })
}

pub(crate) fn remove_project_build_cache(workspace: &Workspace, project: &Project) -> Result<bool> {
    validate_cache_reset_path(workspace)?;

    let build_cache_dir = task_cache_target_dir(workspace, &project.name, Target::Build);
    let removed = match fs::symlink_metadata(&build_cache_dir) {
        Ok(_) => {
            remove_path(&build_cache_dir)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", build_cache_dir.display()));
        }
    };

    Ok(removed)
}

pub(crate) fn prepare_cache(workspace: &Workspace) -> Result<()> {
    if !workspace.cache_dir.exists() {
        return Ok(());
    }

    cleanup_stale_cache_work_dirs(&workspace.cache_dir)?;
    prune_cache(workspace)
}

pub(crate) fn prune_cache(workspace: &Workspace) -> Result<()> {
    if workspace.cache_max_age_seconds.is_none() && workspace.cache_max_size_bytes.is_none() {
        return Ok(());
    }
    if !workspace.cache_dir.exists() {
        return Ok(());
    }

    validate_cache_reset_path(workspace)?;

    let mut retained = Vec::new();
    let now = current_unix_seconds();
    for candidate in cache_prune_candidates(workspace)? {
        let expired = workspace
            .cache_max_age_seconds
            .map(|max_age| now.saturating_sub(candidate.created_at_unix_seconds) > max_age)
            .unwrap_or(false);
        if expired {
            remove_path(&candidate.path)?;
        } else {
            retained.push(candidate);
        }
    }

    let Some(max_size) = workspace.cache_max_size_bytes else {
        return Ok(());
    };

    let mut total_size = retained
        .iter()
        .map(|candidate| candidate.byte_len)
        .sum::<u64>();
    if total_size <= max_size {
        return Ok(());
    }

    retained.sort_by_key(|candidate| candidate.created_at_unix_seconds);
    for candidate in retained {
        remove_path(&candidate.path)?;
        total_size = total_size.saturating_sub(candidate.byte_len);
        if total_size <= max_size {
            break;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachePruneCandidate {
    path: PathBuf,
    byte_len: u64,
    created_at_unix_seconds: u64,
}

fn cache_prune_candidates(workspace: &Workspace) -> Result<Vec<CachePruneCandidate>> {
    let task_root = workspace.cache_dir.join(CACHE_SCHEMA_VERSION).join("task");
    if !task_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for entry in WalkDir::new(&task_root)
        .min_depth(3)
        .max_depth(3)
        .follow_links(false)
    {
        let entry = entry.with_context(|| format!("failed to walk {}", task_root.display()))?;
        if !entry.file_type().is_dir() || !entry.path().join("meta.json").is_file() {
            continue;
        }

        candidates.push(CachePruneCandidate {
            path: entry.path().to_path_buf(),
            byte_len: directory_byte_len(entry.path())?,
            created_at_unix_seconds: cache_entry_created_at(entry.path())?,
        });
    }

    Ok(candidates)
}

fn cleanup_stale_cache_work_dirs(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let mut stale_paths = Vec::new();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_dir() || !is_cache_work_dir_name(entry.file_name()) {
            continue;
        }
        if is_stale_path(entry.path(), STALE_CACHE_WORK_DIR_SECONDS)? {
            stale_paths.push(entry.path().to_path_buf());
        }
    }

    stale_paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in stale_paths {
        remove_path(&path)?;
    }

    Ok(())
}

fn is_cache_work_dir_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with(".tmp-") || name.starts_with(".lock-")
}

fn is_stale_path(path: &Path, max_age_seconds: u64) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let age_seconds = SystemTime::now()
        .duration_since(modified)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    Ok(age_seconds > max_age_seconds)
}

fn directory_byte_len(path: &Path) -> Result<u64> {
    let mut byte_len = 0_u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.with_context(|| format!("failed to walk {}", path.display()))?;
        if entry.file_type().is_file() {
            byte_len += entry
                .metadata()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                .len();
        }
    }
    Ok(byte_len)
}

fn cache_entry_created_at(entry_dir: &Path) -> Result<u64> {
    let metadata_path = entry_dir.join("meta.json");
    if let Ok(metadata_text) = fs::read_to_string(&metadata_path) {
        if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&metadata_text) {
            if let Some(created_at) = metadata
                .get("created_at_unix_seconds")
                .and_then(serde_json::Value::as_u64)
            {
                return Ok(created_at);
            }
        }
    }

    path_modified_unix_seconds(entry_dir)
}

fn path_modified_unix_seconds(path: &Path) -> Result<u64> {
    let modified = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .modified()
        .unwrap_or(UNIX_EPOCH);
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0))
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn validate_cache_reset_path(workspace: &Workspace) -> Result<()> {
    if workspace.cache_dir.as_os_str().is_empty() || workspace.cache_dir.parent().is_none() {
        bail!(
            "refusing to remove unsafe cache directory {}",
            workspace.cache_dir.display()
        );
    }

    if workspace.cache_dir == workspace.root {
        bail!(
            "refusing to remove workspace root as cache directory: {}",
            workspace.cache_dir.display()
        );
    }

    let workspace_root = workspace
        .root
        .canonicalize()
        .unwrap_or(workspace.root.clone());
    if let Ok(cache_dir) = workspace.cache_dir.canonicalize() {
        if cache_dir == workspace_root || cache_dir.parent().is_none() {
            bail!(
                "refusing to remove unsafe cache directory {}",
                workspace.cache_dir.display()
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheEntryMetadata {
    schema_version: String,
    gomo_version: String,
    operating_system: String,
    architecture: String,
    project: String,
    project_root: String,
    project_target: String,
    target: String,
    command: String,
    hash: String,
    gleam_version: String,
    input_globs: Vec<String>,
    stdout: CacheArtifactMetadata,
    stderr: CacheArtifactMetadata,
    cached_folders: Vec<String>,
    output_archive: Option<CacheArtifactMetadata>,
    created_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheArtifactMetadata {
    blake3: String,
    byte_len: u64,
}

pub(crate) fn restore_successful_build(
    workspace: &Workspace,
    project: &Project,
    task_hash: &TaskHash,
) -> Result<Option<CachedTaskExecution>> {
    ensure_build_hash(task_hash)?;

    let entry_dir = task_cache_entry_dir(workspace, task_hash);
    if !is_complete_build_cache_entry(&entry_dir, task_hash)? {
        return Ok(None);
    }

    let archive_path = entry_dir.join("outputs.tar.zst");
    restore_build_outputs(project, task_hash, &archive_path)?;

    Ok(Some(CachedTaskExecution {
        stdout: read_optional_string(&entry_dir.join("stdout.txt"))?,
        stderr: read_optional_string(&entry_dir.join("stderr.txt"))?,
    }))
}

pub(crate) fn restore_successful_test(
    workspace: &Workspace,
    task_hash: &TaskHash,
) -> Result<Option<CachedTaskExecution>> {
    ensure_test_hash(task_hash)?;

    restore_successful_output_task(workspace, task_hash)
}

pub(crate) fn restore_successful_format(
    workspace: &Workspace,
    task_hash: &TaskHash,
) -> Result<Option<CachedTaskExecution>> {
    ensure_format_hash(task_hash)?;

    restore_successful_output_task(workspace, task_hash)
}

fn restore_successful_output_task(
    workspace: &Workspace,
    task_hash: &TaskHash,
) -> Result<Option<CachedTaskExecution>> {
    let entry_dir = task_cache_entry_dir(workspace, task_hash);
    if !is_complete_output_cache_entry(&entry_dir, task_hash)? {
        return Ok(None);
    }

    Ok(Some(CachedTaskExecution {
        stdout: read_optional_string(&entry_dir.join("stdout.txt"))?,
        stderr: read_optional_string(&entry_dir.join("stderr.txt"))?,
    }))
}

pub(crate) fn store_successful_build(
    workspace: &Workspace,
    project: &Project,
    task_hash: &TaskHash,
    execution: &TaskExecution,
) -> Result<()> {
    ensure_build_hash(task_hash)?;
    if !execution.is_success() {
        bail!("failed build task `{}` must not be cached", project.name);
    }

    for folder in &task_hash.cached_folders {
        let output_dir = project.root.join(folder);
        ensure_cached_folder_parent_is_safe(project, folder)?;
        if !is_real_directory(&output_dir)? {
            bail!(
                "successful build task `{}` did not create cached folder {}",
                project.name,
                output_dir.display()
            );
        }
    }

    let entry_dir = task_cache_entry_dir(workspace, task_hash);
    if is_complete_build_cache_entry(&entry_dir, task_hash)? {
        return Ok(());
    }

    let parent = entry_dir
        .parent()
        .context("cache entry should have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    cleanup_stale_cache_work_dirs(parent)?;

    let _lock = match acquire_cache_entry_lock(
        parent,
        task_hash,
        &entry_dir,
        is_complete_build_cache_entry,
    )? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    if is_complete_build_cache_entry(&entry_dir, task_hash)? {
        return Ok(());
    }
    if entry_dir.exists() {
        remove_path(&entry_dir)?;
    }

    let temp_dir = parent.join(format!(".tmp-{}-{}", task_hash.hash, unique_suffix()));
    if temp_dir.exists() {
        remove_path(&temp_dir)?;
    }
    fs::create_dir(&temp_dir).with_context(|| {
        format!(
            "failed to create cache temp directory {}",
            temp_dir.display()
        )
    })?;

    let write_result = (|| -> Result<()> {
        let stdout = write_cache_text_artifact(&temp_dir.join("stdout.txt"), &execution.stdout)
            .with_context(|| format!("failed to write cached stdout for `{}`", project.name))?;
        let stderr = write_cache_text_artifact(&temp_dir.join("stderr.txt"), &execution.stderr)
            .with_context(|| format!("failed to write cached stderr for `{}`", project.name))?;
        let archive_path = temp_dir.join("outputs.tar.zst");
        write_build_outputs_archive(&archive_path, project, &task_hash.cached_folders)
            .with_context(|| format!("failed to archive build outputs for `{}`", project.name))?;
        let output_archive = hash_cache_artifact(&archive_path)?;
        write_cache_metadata(
            &temp_dir.join("meta.json"),
            task_hash,
            stdout,
            stderr,
            Some(output_archive),
        )
        .with_context(|| format!("failed to write cache metadata for `{}`", project.name))?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }

    if is_complete_build_cache_entry(&entry_dir, task_hash)? {
        let _ = fs::remove_dir_all(&temp_dir);
        return Ok(());
    }

    fs::rename(&temp_dir, &entry_dir).with_context(|| {
        format!(
            "failed to move cache entry from {} to {}",
            temp_dir.display(),
            entry_dir.display()
        )
    })?;

    Ok(())
}

pub(crate) fn store_successful_test(
    workspace: &Workspace,
    project: &Project,
    task_hash: &TaskHash,
    execution: &TaskExecution,
) -> Result<()> {
    ensure_test_hash(task_hash)?;
    store_successful_output_task(workspace, project, task_hash, execution, "test")
}

pub(crate) fn store_successful_format(
    workspace: &Workspace,
    project: &Project,
    task_hash: &TaskHash,
    execution: &TaskExecution,
) -> Result<()> {
    ensure_format_hash(task_hash)?;
    store_successful_output_task(workspace, project, task_hash, execution, "format")
}

fn store_successful_output_task(
    workspace: &Workspace,
    project: &Project,
    task_hash: &TaskHash,
    execution: &TaskExecution,
    target_name: &str,
) -> Result<()> {
    if !execution.is_success() {
        bail!(
            "failed {target_name} task `{}` must not be cached",
            project.name
        );
    }

    let entry_dir = task_cache_entry_dir(workspace, task_hash);
    if is_complete_output_cache_entry(&entry_dir, task_hash)? {
        return Ok(());
    }

    let parent = entry_dir
        .parent()
        .context("cache entry should have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    cleanup_stale_cache_work_dirs(parent)?;

    let _lock = match acquire_cache_entry_lock(
        parent,
        task_hash,
        &entry_dir,
        is_complete_output_cache_entry,
    )? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    if is_complete_output_cache_entry(&entry_dir, task_hash)? {
        return Ok(());
    }
    if entry_dir.exists() {
        remove_path(&entry_dir)?;
    }

    let temp_dir = parent.join(format!(".tmp-{}-{}", task_hash.hash, unique_suffix()));
    if temp_dir.exists() {
        remove_path(&temp_dir)?;
    }
    fs::create_dir(&temp_dir).with_context(|| {
        format!(
            "failed to create cache temp directory {}",
            temp_dir.display()
        )
    })?;

    let write_result = (|| -> Result<()> {
        let stdout = write_cache_text_artifact(&temp_dir.join("stdout.txt"), &execution.stdout)
            .with_context(|| format!("failed to write cached stdout for `{}`", project.name))?;
        let stderr = write_cache_text_artifact(&temp_dir.join("stderr.txt"), &execution.stderr)
            .with_context(|| format!("failed to write cached stderr for `{}`", project.name))?;
        write_cache_metadata(&temp_dir.join("meta.json"), task_hash, stdout, stderr, None)
            .with_context(|| format!("failed to write cache metadata for `{}`", project.name))?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }

    if is_complete_output_cache_entry(&entry_dir, task_hash)? {
        let _ = fs::remove_dir_all(&temp_dir);
        return Ok(());
    }

    fs::rename(&temp_dir, &entry_dir).with_context(|| {
        format!(
            "failed to move cache entry from {} to {}",
            temp_dir.display(),
            entry_dir.display()
        )
    })?;

    Ok(())
}

fn write_cache_text_artifact(path: &Path, contents: &str) -> Result<CacheArtifactMetadata> {
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    hash_cache_artifact(path)
}

fn write_cache_metadata(
    path: &Path,
    task_hash: &TaskHash,
    stdout: CacheArtifactMetadata,
    stderr: CacheArtifactMetadata,
    output_archive: Option<CacheArtifactMetadata>,
) -> Result<()> {
    let metadata = CacheEntryMetadata::from_task_hash(task_hash, stdout, stderr, output_archive);
    let metadata_json =
        serde_json::to_string_pretty(&metadata).context("failed to serialize cache metadata")?;
    fs::write(path, metadata_json).with_context(|| format!("failed to write {}", path.display()))
}

struct CacheEntryLock {
    path: PathBuf,
}

impl Drop for CacheEntryLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn acquire_cache_entry_lock(
    parent: &Path,
    task_hash: &TaskHash,
    entry_dir: &Path,
    is_complete: fn(&Path, &TaskHash) -> Result<bool>,
) -> Result<Option<CacheEntryLock>> {
    let lock_dir = parent.join(format!(".lock-{}", task_hash.hash));

    for attempt in 0..=CACHE_STORE_LOCK_ATTEMPTS {
        match fs::create_dir(&lock_dir) {
            Ok(()) => return Ok(Some(CacheEntryLock { path: lock_dir })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_complete(entry_dir, task_hash)? {
                    return Ok(None);
                }
                if is_stale_path(&lock_dir, STALE_CACHE_WORK_DIR_SECONDS)? {
                    remove_path(&lock_dir)?;
                    continue;
                }
                if attempt == CACHE_STORE_LOCK_ATTEMPTS {
                    bail!(
                        "timed out waiting for cache entry lock {}",
                        lock_dir.display()
                    );
                }
                thread::sleep(CACHE_STORE_LOCK_RETRY_DELAY);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create cache lock {}", lock_dir.display())
                });
            }
        }
    }

    bail!(
        "timed out waiting for cache entry lock {}",
        lock_dir.display()
    )
}

pub(crate) fn task_cache_entry_dir(workspace: &Workspace, task_hash: &TaskHash) -> PathBuf {
    task_cache_target_dir(workspace, &task_hash.project, task_hash.target)
        .join(cache_path_component(&task_hash.hash))
}

fn task_cache_target_dir(workspace: &Workspace, project: &str, target: Target) -> PathBuf {
    workspace
        .cache_dir
        .join(CACHE_SCHEMA_VERSION)
        .join("task")
        .join(cache_path_component(project))
        .join(cache_path_component(target.as_str()))
}

impl CacheEntryMetadata {
    fn from_task_hash(
        task_hash: &TaskHash,
        stdout: CacheArtifactMetadata,
        stderr: CacheArtifactMetadata,
        output_archive: Option<CacheArtifactMetadata>,
    ) -> Self {
        Self {
            schema_version: task_hash.schema_version.clone(),
            gomo_version: task_hash.gomo_version.clone(),
            operating_system: task_hash.operating_system.clone(),
            architecture: task_hash.architecture.clone(),
            project: task_hash.project.clone(),
            project_root: task_hash.project_root.clone(),
            project_target: task_hash.project_target.clone(),
            target: task_hash.target.as_str().to_string(),
            command: task_hash.command.clone(),
            hash: task_hash.hash.clone(),
            gleam_version: task_hash.gleam_version.clone(),
            input_globs: task_hash.input_globs.clone(),
            cached_folders: task_hash.cached_folders.clone(),
            stdout,
            stderr,
            output_archive,
            created_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        }
    }

    fn matches_task_hash(&self, task_hash: &TaskHash) -> bool {
        self.schema_version == task_hash.schema_version
            && self.gomo_version == task_hash.gomo_version
            && self.operating_system == task_hash.operating_system
            && self.architecture == task_hash.architecture
            && self.project == task_hash.project
            && self.project_root == task_hash.project_root
            && self.project_target == task_hash.project_target
            && self.target == task_hash.target.as_str()
            && self.command == task_hash.command
            && self.hash == task_hash.hash
            && self.gleam_version == task_hash.gleam_version
            && self.cached_folders == task_hash.cached_folders
    }
}

fn ensure_build_hash(task_hash: &TaskHash) -> Result<()> {
    if task_hash.target == Target::Build {
        return Ok(());
    }

    bail!(
        "target `{}` does not support build cache entries",
        task_hash.target
    )
}

fn ensure_test_hash(task_hash: &TaskHash) -> Result<()> {
    if task_hash.target == Target::Test {
        return Ok(());
    }

    bail!(
        "target `{}` does not support test cache entries",
        task_hash.target
    )
}

fn ensure_format_hash(task_hash: &TaskHash) -> Result<()> {
    if task_hash.target == Target::Format {
        return Ok(());
    }

    bail!(
        "target `{}` does not support format cache entries",
        task_hash.target
    )
}

fn read_valid_metadata(
    entry_dir: &Path,
    task_hash: &TaskHash,
) -> Result<Option<CacheEntryMetadata>> {
    if !entry_dir.is_dir() {
        return Ok(None);
    }

    let metadata_path = entry_dir.join("meta.json");
    let metadata_text = match fs::read_to_string(&metadata_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", metadata_path.display()));
        }
    };

    let metadata = match serde_json::from_str::<CacheEntryMetadata>(&metadata_text) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };

    if metadata.matches_task_hash(task_hash) {
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

fn read_optional_string(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn is_complete_build_cache_entry(entry_dir: &Path, task_hash: &TaskHash) -> Result<bool> {
    let Some(metadata) = read_valid_metadata(entry_dir, task_hash)? else {
        return Ok(false);
    };
    let Some(output_archive) = metadata.output_archive.as_ref() else {
        return Ok(false);
    };

    Ok(
        artifact_matches(&entry_dir.join("stdout.txt"), &metadata.stdout)?
            && artifact_matches(&entry_dir.join("stderr.txt"), &metadata.stderr)?
            && artifact_matches(&entry_dir.join("outputs.tar.zst"), output_archive)?,
    )
}

fn is_complete_output_cache_entry(entry_dir: &Path, task_hash: &TaskHash) -> Result<bool> {
    let Some(metadata) = read_valid_metadata(entry_dir, task_hash)? else {
        return Ok(false);
    };

    Ok(metadata.output_archive.is_none()
        && artifact_matches(&entry_dir.join("stdout.txt"), &metadata.stdout)?
        && artifact_matches(&entry_dir.join("stderr.txt"), &metadata.stderr)?)
}

fn artifact_matches(path: &Path, expected: &CacheArtifactMetadata) -> Result<bool> {
    if !is_real_file(path)? {
        return Ok(false);
    }

    Ok(&hash_cache_artifact(path)? == expected)
}

fn hash_cache_artifact(path: &Path) -> Result<CacheArtifactMetadata> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        byte_len += bytes_read as u64;
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(CacheArtifactMetadata {
        blake3: hasher.finalize().to_hex().to_string(),
        byte_len,
    })
}

fn write_build_outputs_archive(
    archive_path: &Path,
    project: &Project,
    cached_folders: &[String],
) -> Result<()> {
    let archive_file = File::create(archive_path)
        .with_context(|| format!("failed to create {}", archive_path.display()))?;
    let encoder = zstd::stream::write::Encoder::new(archive_file, 0)
        .context("failed to create zstd encoder")?;
    let mut archive = tar::Builder::new(encoder);
    let cached_output_dirs = cached_folders
        .iter()
        .map(|folder| {
            let output_dir = project.root.join(folder);
            let canonical_dir = output_dir
                .canonicalize()
                .with_context(|| format!("failed to resolve cached folder {folder}"))?;
            Ok((canonical_dir, PathBuf::from(folder)))
        })
        .collect::<Result<Vec<(PathBuf, PathBuf)>>>()?;
    for folder in cached_folders {
        let output_dir = project.root.join(folder);
        append_cached_output_tree(
            &mut archive,
            &output_dir,
            Path::new(folder),
            &cached_output_dirs,
        )?;
    }
    let encoder = archive
        .into_inner()
        .context("failed to finish tar archive")?;
    encoder.finish().context("failed to finish zstd archive")?;
    Ok(())
}

fn append_cached_output_tree<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    archive_root: &Path,
    cached_output_dirs: &[(PathBuf, PathBuf)],
) -> Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        let relative_path = entry.path().strip_prefix(root).with_context(|| {
            format!(
                "failed to compute relative path for {} from {}",
                entry.path().display(),
                root.display()
            )
        })?;
        let archive_path = archive_root.join(relative_path);

        if entry.file_type().is_symlink() {
            let target = entry.path().canonicalize().with_context(|| {
                format!(
                    "failed to resolve cached output symlink {}",
                    entry.path().display()
                )
            })?;
            let target_archive_path = cached_output_dirs
                .iter()
                .find_map(|(output_dir, output_archive_root)| {
                    target
                        .strip_prefix(output_dir)
                        .ok()
                        .map(|relative_target| output_archive_root.join(relative_target))
                })
                .with_context(|| {
                    format!(
                        "cached output symlink {} resolves outside configured cached folders",
                        entry.path().display()
                    )
                })?;
            let link_parent = archive_path.parent().unwrap_or(Path::new(""));
            let link_target = relative_path_between(link_parent, &target_archive_path)?;
            let metadata = fs::symlink_metadata(entry.path())
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            archive
                .append_link(&mut header, &archive_path, &link_target)
                .with_context(|| {
                    format!(
                        "failed to append cached output symlink {}",
                        entry.path().display()
                    )
                })?;
        } else {
            archive
                .append_path_with_name(entry.path(), &archive_path)
                .with_context(|| {
                    format!(
                        "failed to append {} to output archive",
                        entry.path().display()
                    )
                })?;
        }
    }
    Ok(())
}

fn relative_path_between(from: &Path, to: &Path) -> Result<PathBuf> {
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    if from_components
        .iter()
        .chain(&to_components)
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("cannot compute relative path between archive paths");
    }

    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Ok(relative)
}

fn ensure_cached_folder_parent_is_safe(project: &Project, folder: &str) -> Result<()> {
    let components = Path::new(folder).components().collect::<Vec<_>>();
    let mut current = project.root.clone();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "cached folder {} has symlink parent {}",
                    project.root.join(folder).display(),
                    current.display()
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!(
                    "cached folder {} has non-directory parent {}",
                    project.root.join(folder).display(),
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

fn restore_build_outputs(
    project: &Project,
    task_hash: &TaskHash,
    archive_path: &Path,
) -> Result<()> {
    let temp_dir = project
        .root
        .join(format!("{RESTORE_WORK_DIR_PREFIX}{}", unique_suffix()));
    if temp_dir.exists() {
        remove_path(&temp_dir)?;
    }
    fs::create_dir(&temp_dir).with_context(|| {
        format!(
            "failed to create build output restore temp directory {}",
            temp_dir.display()
        )
    })?;

    let restore_result = (|| -> Result<()> {
        unpack_build_outputs_archive(archive_path, &temp_dir, &task_hash.cached_folders)?;
        install_restored_build_outputs(project, &temp_dir, &task_hash.cached_folders)
    })();

    let cleanup_result = if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("failed to remove {}", temp_dir.display()))
    } else {
        Ok(())
    };

    restore_result.and(cleanup_result)
}

fn unpack_build_outputs_archive(
    archive_path: &Path,
    temp_dir: &Path,
    cached_folders: &[String],
) -> Result<()> {
    let archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder =
        zstd::stream::read::Decoder::new(archive_file).context("failed to create zstd decoder")?;
    let mut archive = tar::Archive::new(decoder);
    let mut symlinks = Vec::new();

    for entry in archive
        .entries()
        .context("failed to read build output archive")?
    {
        let mut entry = entry.context("failed to read build output archive entry")?;
        let relative_path = entry
            .path()
            .context("failed to read build output archive path")?
            .into_owned();
        validate_build_output_archive_path(&relative_path, cached_folders)?;

        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() {
            let link_target = entry
                .link_name()
                .context("failed to read cached build symlink target")?
                .context("cached build symlink is missing its target")?
                .into_owned();
            let resolved_target =
                resolve_archive_symlink_target(&relative_path, &link_target, cached_folders)?;
            symlinks.push((relative_path, link_target, resolved_target));
            continue;
        }
        if !(entry_type.is_file() || entry_type.is_dir()) {
            bail!(
                "cached build output archive contains unsupported entry type at {}",
                relative_path.display()
            );
        }

        let destination = temp_dir.join(&relative_path);
        entry.unpack(&destination).with_context(|| {
            format!(
                "failed to unpack cached build entry {} to {}",
                relative_path.display(),
                destination.display()
            )
        })?;
    }

    for (relative_path, link_target, resolved_target) in symlinks {
        if !temp_dir.join(&resolved_target).exists() {
            bail!(
                "cached build symlink {} points to missing target {}",
                relative_path.display(),
                resolved_target.display()
            );
        }
        let destination = temp_dir.join(&relative_path);
        create_symlink(&link_target, &destination, &temp_dir.join(&resolved_target))?;
    }

    Ok(())
}

fn resolve_archive_symlink_target(
    symlink_path: &Path,
    link_target: &Path,
    cached_folders: &[String],
) -> Result<PathBuf> {
    if link_target.is_absolute() {
        bail!(
            "cached build symlink {} has absolute target {}",
            symlink_path.display(),
            link_target.display()
        );
    }

    let mut resolved = symlink_path.parent().unwrap_or(Path::new("")).to_path_buf();
    for component in link_target.components() {
        match component {
            Component::Normal(value) => resolved.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    bail!(
                        "cached build symlink {} leaves configured cached folders",
                        symlink_path.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "cached build symlink {} has invalid target {}",
                    symlink_path.display(),
                    link_target.display()
                );
            }
        }
    }
    validate_build_output_archive_path(&resolved, cached_folders).with_context(|| {
        format!(
            "cached build symlink {} leaves configured cached folders",
            symlink_path.display()
        )
    })?;
    Ok(resolved)
}

#[cfg(unix)]
fn create_symlink(link_target: &Path, destination: &Path, _resolved_target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link_target, destination).with_context(|| {
        format!(
            "failed to create cached build symlink {} -> {}",
            destination.display(),
            link_target.display()
        )
    })
}

#[cfg(windows)]
fn create_symlink(link_target: &Path, destination: &Path, resolved_target: &Path) -> Result<()> {
    let result = if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(link_target, destination)
    } else {
        std::os::windows::fs::symlink_file(link_target, destination)
    };
    result.with_context(|| {
        format!(
            "failed to create cached build symlink {} -> {}",
            destination.display(),
            link_target.display()
        )
    })
}

fn validate_build_output_archive_path(path: &Path, cached_folders: &[String]) -> Result<()> {
    if path.is_absolute() {
        bail!(
            "cached build output archive path {} is absolute",
            path.display()
        );
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!(
                "cached build output archive path {} contains an invalid component",
                path.display()
            );
        }
    }

    if !cached_folders
        .iter()
        .any(|folder| path.starts_with(Path::new(folder)))
    {
        bail!(
            "cached build output archive path {} is not under a configured cached folder",
            path.display()
        );
    }

    Ok(())
}

fn install_restored_build_outputs(
    project: &Project,
    temp_dir: &Path,
    cached_folders: &[String],
) -> Result<()> {
    for folder in cached_folders {
        ensure_cached_folder_parent_is_safe(project, folder)?;
        if !is_real_directory(&temp_dir.join(folder))? {
            bail!("cached build output archive did not contain {folder}/");
        }
    }

    let backup_root = temp_dir.join(format!(".gomo-backups-{}", unique_suffix()));
    fs::create_dir(&backup_root)
        .with_context(|| format!("failed to create {}", backup_root.display()))?;
    let mut backed_up = Vec::new();
    let mut installed = Vec::new();

    let install_result = (|| -> Result<()> {
        for (index, folder) in cached_folders.iter().enumerate() {
            let destination = project.root.join(folder);
            if destination.exists() || fs::symlink_metadata(&destination).is_ok() {
                let backup = backup_root.join(index.to_string());
                fs::rename(&destination, &backup).with_context(|| {
                    format!(
                        "failed to move existing cached folder from {} to {}",
                        destination.display(),
                        backup.display()
                    )
                })?;
                backed_up.push((destination.clone(), backup));
            }
        }

        for folder in cached_folders {
            let restored = temp_dir.join(folder);
            let destination = project.root.join(folder);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create output parent {}", parent.display())
                })?;
            }
            fs::rename(&restored, &destination).with_context(|| {
                format!(
                    "failed to restore cached folder from {} to {}",
                    restored.display(),
                    destination.display()
                )
            })?;
            installed.push(destination);
        }
        Ok(())
    })();

    if let Err(error) = install_result {
        for destination in installed.iter().rev() {
            let _ = remove_path(destination);
        }
        for (destination, backup) in backed_up.iter().rev() {
            if !destination.exists() {
                let _ = fs::rename(backup, destination);
            }
        }
        return Err(error);
    }

    remove_path(&backup_root)?;
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
    }
}

fn is_real_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn is_real_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn cache_path_component(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        return value.to_string();
    }

    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::runner::TaskExecution;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    const GLEAM_VERSION: &str = "gleam 1.0.0";

    fn write_dependency_fixture(test_workspace: &TestWorkspace) {
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
shared = { path = "../../libs/shared" }
"#,
        );
        test_workspace.write_file("apps/demo/src/main.gleam", "pub fn main() { Nil }\n");
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
    }

    fn load_workspace(test_workspace: &TestWorkspace) -> (Workspace, ProjectGraph) {
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        (workspace, graph)
    }

    fn project<'a>(workspace: &'a Workspace, name: &str) -> &'a Project {
        workspace
            .projects
            .iter()
            .find(|project| project.name == name)
            .expect("project should exist")
    }

    #[test]
    fn computes_stable_hashes_for_unchanged_tasks() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        write_dependency_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);
        let demo = project(&workspace, "demo");

        let first = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("hash should compute");
        let second = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("hash should compute again");

        assert_eq!(first.hash, second.hash);
        assert_eq!(first.dependency_hashes.len(), 1);
        assert_eq!(first.dependency_hashes[0].project, "shared");
        assert_eq!(first.dependency_hashes[0].target, Target::Build);
        assert!(first.input_globs.contains(&"gleam.toml".to_string()));
        assert!(first.input_globs.contains(&"src/**".to_string()));
        assert!(
            first
                .input_files
                .iter()
                .any(|input| input.relative_path == "src/main.gleam")
        );
    }

    #[test]
    fn source_changes_invalidate_own_and_downstream_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        write_dependency_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let demo = project(&workspace, "demo");

        let shared_before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("shared hash should compute");
        let demo_before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("demo hash should compute");

        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 2 }\n");

        let shared_after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("shared hash should recompute");
        let demo_after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("demo hash should recompute");

        assert_ne!(shared_before.hash, shared_after.hash);
        assert_ne!(demo_before.hash, demo_after.hash);
        assert_eq!(demo_after.dependency_hashes[0].hash, shared_after.hash);
    }

    #[test]
    fn path_dependencies_outside_project_roots_participate_in_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
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
        test_workspace.write_file("apps/demo/src/main.gleam", "pub fn main() { Nil }\n");
        test_workspace.write_manifest(
            "tools/esgleam",
            r#"
name = "esgleam"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("tools/esgleam/src/main.gleam", "pub fn value() { 1 }\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let demo = project(&workspace, "demo");

        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("demo hash should compute");

        test_workspace.write_file("tools/esgleam/src/main.gleam", "pub fn value() { 2 }\n");

        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("demo hash should recompute");

        assert_eq!(before.dependency_hashes[0].project, "esgleam");
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn dev_path_dependencies_affect_tests_but_not_builds() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
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

[dev-dependencies]
test_support = { path = "../../tools/test_support" }
"#,
        );
        test_workspace.write_file("apps/demo/src/main.gleam", "pub fn main() { Nil }\n");
        test_workspace.write_file("apps/demo/test/main_test.gleam", "pub fn test() { Nil }\n");
        test_workspace.write_manifest(
            "tools/test_support",
            r#"
name = "test_support"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "tools/test_support/src/main.gleam",
            "pub fn support() { 1 }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let demo = project(&workspace, "demo");

        let build_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");
        let test_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        assert!(build_hash.dependency_hashes.is_empty());
        assert_eq!(test_hash.dependency_hashes[0].project, "test_support");

        test_workspace.write_file(
            "tools/test_support/src/main.gleam",
            "pub fn support() { 2 }\n",
        );
        let default_build_after_dev_change = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("default build hash should recompute");
        let test_after_dev_change = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should recompute");

        assert_eq!(build_hash.hash, default_build_after_dev_change.hash);
        assert_ne!(test_hash.hash, test_after_dev_change.hash);

        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dev-dependencies]
test_support = { path = "../../tools/test_support" }

[tools.gomo.build]
command = "make build"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let demo = project(&workspace, "demo");
        let custom_build_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            demo,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("custom build hash should compute");

        assert_eq!(
            custom_build_hash.dependency_hashes[0].project,
            "test_support"
        );
    }

    #[test]
    fn test_inputs_do_not_affect_build_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test() { Nil }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let build_before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");
        let test_before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        test_workspace.write_file("libs/shared/test/main_test.gleam", "pub fn test() { 1 }\n");

        let build_after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should recompute");
        let test_after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should recompute");

        assert_eq!(build_before.hash, build_after.hash);
        assert_ne!(test_before.hash, test_after.hash);
    }

    #[test]
    fn target_input_overrides_participate_in_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.test]
inputs = ["gleam.toml", "src/**", "test/**", "fixtures/**"]
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test() { Nil }\n",
        );
        test_workspace.write_file("libs/shared/fixtures/value.txt", "one\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        test_workspace.write_file("libs/shared/fixtures/value.txt", "two\n");

        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should recompute");

        assert_eq!(before.input_source, InputGlobSource::ProjectOverride);
        assert!(before.input_globs.contains(&"fixtures/**".to_string()));
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn workspace_inputs_participate_in_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]

[workspace.test]
inputs = ["devenv.nix"]
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file("devenv.nix", "{ pkgs, ... }: {}\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        test_workspace.write_file("devenv.nix", "{ pkgs, ... }: { env.FOO = \"bar\"; }\n");

        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should recompute");

        assert_eq!(before.workspace_input_globs, ["devenv.nix"]);
        assert_eq!(before.workspace_input_files[0].relative_path, "devenv.nix");
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn cache_work_and_output_directories_do_not_participate_in_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]

[workspace.build]
inputs = ["libs/**"]
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.build]
inputs = ["**"]
cached_folders = ["dist"]
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");

        test_workspace.write_file(
            "libs/shared/.gomo-restore-test/build/dev/erlang/shared/artifact.erl",
            "transient\n",
        );
        test_workspace.write_file(
            "libs/shared/dist/dev/erlang/shared/artifact.erl",
            "compiled\n",
        );

        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should ignore cache work and output directories");

        assert_eq!(before.hash, after.hash);
        assert_eq!(before.input_files, after.input_files);
        assert_eq!(before.workspace_input_files, after.workspace_input_files);
    }

    #[test]
    fn custom_target_commands_participate_in_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.test]
command = "gleam test --target erlang"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.test]
command = "gleam test --target javascript"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should recompute");

        assert_eq!(before.command, "gleam test --target erlang");
        assert_eq!(after.command, "gleam test --target javascript");
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn stores_and_restores_successful_build_entries() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/build/dev/erlang/shared/_gleam_artefacts/shared.erl",
            "compiled\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");
        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        fs::create_dir_all(&entry_dir).expect("incomplete cache dir should be created");
        fs::write(entry_dir.join("meta.json"), "{}")
            .expect("incomplete metadata should be written");

        store_successful_build(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("built\n", "warning\n"),
        )
        .expect("successful build should be cached");

        assert!(entry_dir.join("meta.json").is_file());
        assert!(entry_dir.join("stdout.txt").is_file());
        assert!(entry_dir.join("stderr.txt").is_file());
        assert!(entry_dir.join("outputs.tar.zst").is_file());

        fs::remove_dir_all(shared.root.join("build")).expect("build dir should be removed");

        let cached = restore_successful_build(&workspace, shared, &task_hash)
            .expect("cache restore should succeed")
            .expect("cache entry should hit");

        assert_eq!(cached.stdout, "built\n");
        assert_eq!(cached.stderr, "warning\n");
        assert_eq!(
            fs::read_to_string(
                shared
                    .root
                    .join("build/dev/erlang/shared/_gleam_artefacts/shared.erl")
            )
            .expect("restored build artifact should be readable"),
            "compiled\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_internal_build_symlinks_when_restoring() {
        use std::os::unix::fs::symlink;

        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/build/packages/lustre/priv/static.txt",
            "asset\n",
        );
        fs::create_dir_all(
            test_workspace
                .path()
                .join("libs/shared/build/dev/javascript/lustre"),
        )
        .expect("symlink parent should be created");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        symlink(
            shared.root.join("build/packages/lustre/priv"),
            shared.root.join("build/dev/javascript/lustre/priv"),
        )
        .expect("internal build symlink should be created");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");

        store_successful_build(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("built\n", ""),
        )
        .expect("internal build symlink should be cached");
        fs::remove_dir_all(shared.root.join("build")).expect("build should be removed");

        restore_successful_build(&workspace, shared, &task_hash)
            .expect("cache restore should succeed")
            .expect("cache entry should hit");

        let restored_link = shared.root.join("build/dev/javascript/lustre/priv");
        assert!(
            fs::symlink_metadata(&restored_link)
                .expect("restored path should exist")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&restored_link).expect("restored symlink target should be readable"),
            Path::new("../../../packages/lustre/priv")
        );
        assert_eq!(
            restored_link
                .canonicalize()
                .expect("restored symlink should resolve"),
            shared
                .root
                .join("build/packages/lustre/priv")
                .canonicalize()
                .expect("restored target should resolve")
        );
        assert_eq!(
            fs::read_to_string(restored_link.join("static.txt"))
                .expect("symlinked contents should be readable"),
            "asset\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_build_symlinks_outside_cached_folders() {
        use std::os::unix::fs::symlink;

        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        fs::create_dir_all(test_workspace.path().join("libs/shared/build"))
            .expect("build directory should be created");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        symlink(
            shared.root.join("src/main.gleam"),
            shared.root.join("build/source.gleam"),
        )
        .expect("escaping build symlink should be created");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");

        let error = store_successful_build(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("built\n", ""),
        )
        .expect_err("escaping build symlink should be rejected");

        assert!(
            format!("{error:#}").contains("resolves outside configured cached folders"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn stores_and_replaces_multiple_cached_build_folders() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/web",
            r#"
name = "web"
version = "0.1.0"

[tools.gomo.build]
cached_folders = ["build", "dist"]
"#,
        );
        test_workspace.write_file("apps/web/src/main.gleam", "pub fn main() { Nil }\n");
        test_workspace.write_file("apps/web/build/app.mjs", "compiled\n");
        test_workspace.write_file("apps/web/dist/app.js", "bundled\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let web = project(&workspace, "web");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            web,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");

        store_successful_build(
            &workspace,
            web,
            &task_hash,
            &TaskExecution::success("built\n", ""),
        )
        .expect("build outputs should be cached");

        fs::remove_dir_all(web.root.join("build")).expect("build should be removed");
        fs::write(web.root.join("dist/app.js"), "stale\n").expect("dist should become stale");
        fs::write(web.root.join("dist/stale.js"), "stale\n").expect("stale file should exist");

        restore_successful_build(&workspace, web, &task_hash)
            .expect("cache restore should succeed")
            .expect("cache entry should hit");

        assert_eq!(
            fs::read_to_string(web.root.join("build/app.mjs")).expect("build should restore"),
            "compiled\n"
        );
        assert_eq!(
            fs::read_to_string(web.root.join("dist/app.js")).expect("dist should restore"),
            "bundled\n"
        );
        assert!(!web.root.join("dist/stale.js").exists());
    }

    #[test]
    fn cached_folder_configuration_participates_in_build_hashes() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/web",
            r#"
name = "web"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("apps/web/src/main.gleam", "pub fn main() { Nil }\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let before = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            project(&workspace, "web"),
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("default build hash should compute");

        test_workspace.write_manifest(
            "apps/web",
            r#"
name = "web"
version = "0.1.0"

[tools.gomo.build]
cached_folders = ["build", "dist"]
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let after = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            project(&workspace, "web"),
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("custom build hash should compute");

        assert_eq!(before.cached_folders, vec!["build"]);
        assert_eq!(after.cached_folders, vec!["build", "dist"]);
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn stores_and_restores_successful_test_entries() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test_value() { Nil }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");
        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        fs::create_dir_all(&entry_dir).expect("incomplete cache dir should be created");
        fs::write(entry_dir.join("meta.json"), "{}")
            .expect("incomplete metadata should be written");

        store_successful_test(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("tests passed\n", "warning\n"),
        )
        .expect("successful test should be cached");

        assert!(entry_dir.join("meta.json").is_file());
        assert!(entry_dir.join("stdout.txt").is_file());
        assert!(entry_dir.join("stderr.txt").is_file());
        assert!(!entry_dir.join("outputs.tar.zst").exists());

        let cached = restore_successful_test(&workspace, &task_hash)
            .expect("cache restore should succeed")
            .expect("cache entry should hit");

        assert_eq!(cached.stdout, "tests passed\n");
        assert_eq!(cached.stderr, "warning\n");
    }

    #[test]
    fn ignores_cached_output_entries_with_corrupted_artifacts() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test_value() { Nil }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        store_successful_test(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("tests passed\n", "warning\n"),
        )
        .expect("successful test should be cached");

        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        fs::write(entry_dir.join("stdout.txt"), "corrupted\n")
            .expect("cached stdout should be overwritten");

        assert!(
            restore_successful_test(&workspace, &task_hash)
                .expect("cache lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn ignores_cached_build_entries_with_corrupted_archives() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/build/dev/erlang/shared/_gleam_artefacts/shared.erl",
            "compiled\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");

        store_successful_build(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("built\n", "warning\n"),
        )
        .expect("successful build should be cached");

        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        fs::write(entry_dir.join("outputs.tar.zst"), "corrupted\n")
            .expect("cached archive should be overwritten");

        assert!(
            restore_successful_build(&workspace, shared, &task_hash)
                .expect("cache lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn stores_and_restores_successful_format_entries() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Format,
            GLEAM_VERSION,
        )
        .expect("format hash should compute");
        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        fs::create_dir_all(&entry_dir).expect("incomplete cache dir should be created");
        fs::write(entry_dir.join("meta.json"), "{}")
            .expect("incomplete metadata should be written");

        store_successful_format(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("formatted\n", "warning\n"),
        )
        .expect("successful format should be cached");

        assert!(entry_dir.join("meta.json").is_file());
        assert!(entry_dir.join("stdout.txt").is_file());
        assert!(entry_dir.join("stderr.txt").is_file());
        assert!(!entry_dir.join("outputs.tar.zst").exists());

        let cached = restore_successful_format(&workspace, &task_hash)
            .expect("cache restore should succeed")
            .expect("cache entry should hit");

        assert_eq!(cached.stdout, "formatted\n");
        assert_eq!(cached.stderr, "warning\n");
    }

    #[test]
    fn prunes_cache_entries_over_the_configured_size() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]

[cache]
max_age_days = 0
max_size_bytes = 1
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test_value() { Nil }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        store_successful_test(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::success("tests passed\n", "warning\n"),
        )
        .expect("successful test should be cached");
        let entry_dir = task_cache_entry_dir(&workspace, &task_hash);
        assert!(entry_dir.exists());

        prune_cache(&workspace).expect("cache pruning should succeed");

        assert!(!entry_dir.exists());
    }

    #[test]
    fn rejects_failed_test_cache_entries() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/shared/test/main_test.gleam",
            "pub fn test_value() { Nil }\n",
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Test,
            GLEAM_VERSION,
        )
        .expect("test hash should compute");

        let error = store_successful_test(
            &workspace,
            shared,
            &task_hash,
            &TaskExecution::failure(1, "tests failed\n", ""),
        )
        .expect_err("failed tests should not be cached");

        assert!(
            error
                .to_string()
                .contains("failed test task `shared` must not be cached")
        );
        assert!(
            restore_successful_test(&workspace, &task_hash)
                .expect("cache lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn validates_cached_build_output_archive_paths() {
        let folders = vec!["build".to_string(), "dist".to_string()];
        assert!(
            validate_build_output_archive_path(Path::new("build/output.txt"), &folders).is_ok()
        );
        assert!(validate_build_output_archive_path(Path::new("dist/app.js"), &folders).is_ok());
        assert!(validate_build_output_archive_path(Path::new("src/output.txt"), &folders).is_err());
        assert!(
            validate_build_output_archive_path(Path::new("build/../output.txt"), &folders).is_err()
        );
        assert!(
            validate_build_output_archive_path(Path::new("/build/output.txt"), &folders).is_err()
        );
        assert_eq!(
            resolve_archive_symlink_target(
                Path::new("build/dev/javascript/lustre/priv"),
                Path::new("../../../packages/lustre/priv"),
                &folders,
            )
            .expect("internal symlink target should resolve"),
            Path::new("build/packages/lustre/priv")
        );
        assert!(
            resolve_archive_symlink_target(
                Path::new("build/dev/javascript/lustre/priv"),
                Path::new("../../../../src"),
                &folders,
            )
            .is_err()
        );
        assert!(
            resolve_archive_symlink_target(
                Path::new("build/dev/javascript/lustre/priv"),
                Path::new("/etc/passwd"),
                &folders,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_globs_that_leave_the_project_root() {
        let test_workspace = TestWorkspace::new("gomo-cache-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.build]
inputs = ["../outside"]
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);
        let shared = project(&workspace, "shared");

        let error = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            shared,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect_err("invalid glob should fail");

        assert!(
            error
                .to_string()
                .contains("invalid input glob `../outside`")
        );
    }
}
