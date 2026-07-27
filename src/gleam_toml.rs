use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::task::TaskDefinition;

/// Parsed Gomo-relevant data from a package `gleam.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GleamManifest {
    /// Gleam package name.
    pub name: String,
    /// Gleam package version, when declared.
    pub version: Option<String>,
    /// Gleam target, defaulting to `erlang` when omitted.
    pub target: String,
    /// Local path dependencies declared by this package.
    pub path_dependencies: Vec<GleamPathDependency>,
    /// Per-target Gomo config declared under `[tools.gomo.<target>]`.
    pub gomo_targets: BTreeMap<String, GomoTargetConfig>,
    /// Development-process config declared under `[tools.gomo.dev]`.
    pub gomo_dev: GomoDevConfig,
    /// Named tasks declared under `[tools.gomo.tasks.<name>]`.
    pub gomo_tasks: BTreeMap<String, TaskDefinition>,
}

/// Gomo target config parsed from a package `gleam.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GomoTargetConfig {
    /// Optional input glob override for cache keys and affected-file matching.
    pub inputs: Option<Vec<String>>,
    /// Optional command override for a built-in target.
    pub command: Option<String>,
    /// Optional command override for check mode, currently used by format.
    pub check_command: Option<String>,
    /// Optional build output directories to store and restore from cache.
    pub cached_folders: Option<Vec<String>>,
    /// Named task bound to this native target.
    pub task: Option<String>,
}

/// Development-process configuration declared under `[tools.gomo.dev]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GomoDevConfig {
    /// Optional command used to start the long-running development process.
    pub command: Option<String>,
    /// Requested reload strategy. Gomo currently treats hot loading as a
    /// restart fallback until a runtime helper is connected.
    pub reload: GomoReloadStrategy,
}

/// Development process reload strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GomoReloadStrategy {
    #[default]
    Restart,
    Hot,
}

impl std::str::FromStr for GomoReloadStrategy {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim() {
            "restart" => Ok(Self::Restart),
            "hot" => Ok(Self::Hot),
            value => Err(format!("reload must be `restart` or `hot`, got `{value}`")),
        }
    }
}

/// A dependency declared with `{ path = "..." }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GleamPathDependency {
    /// Dependency package name as written in the manifest.
    pub name: String,
    /// Dependency path relative to the manifest's package root.
    pub path: PathBuf,
    /// Manifest dependency table that declared the dependency.
    pub table: DependencyTable,
}

/// Gleam dependency table names that can contain local path dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyTable {
    /// `[dependencies]`.
    Dependencies,
    /// `[dev-dependencies]` or `[dev_dependencies]`.
    DevDependencies,
}

#[derive(Debug, Deserialize)]
struct RawGleamManifest {
    name: String,
    version: Option<String>,
    target: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, RawDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies_dash: BTreeMap<String, RawDependency>,
    #[serde(default, rename = "dev_dependencies")]
    dev_dependencies_underscore: BTreeMap<String, RawDependency>,
    #[serde(default)]
    tools: RawTools,
}

#[derive(Debug, Default, Deserialize)]
struct RawTools {
    #[serde(default)]
    gomo: RawGomoTools,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawGomoTools {
    #[serde(default)]
    build: Option<RawGomoTarget>,
    #[serde(default)]
    format: Option<RawGomoTarget>,
    #[serde(default)]
    test: Option<RawGomoTarget>,
    #[serde(default)]
    watch: Option<RawGomoTarget>,
    #[serde(default)]
    dev: Option<RawGomoDevConfig>,
    #[serde(default)]
    tasks: BTreeMap<String, TaskDefinition>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGomoDevConfig {
    command: Option<String>,
    #[serde(default = "default_reload_strategy")]
    reload: String,
}

fn default_reload_strategy() -> String {
    "restart".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawGomoTarget {
    inputs: Option<Vec<String>>,
    command: Option<String>,
    check: Option<RawGomoTargetCheck>,
    cached_folders: Option<Vec<String>>,
    task: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawGomoTargetCheck {
    command: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDependency {
    Version(String),
    Inline(RawInlineDependency),
}

#[derive(Debug, Deserialize)]
struct RawInlineDependency {
    path: Option<PathBuf>,
}

impl fmt::Display for DependencyTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dependencies => f.write_str("dependencies"),
            Self::DevDependencies => f.write_str("dev-dependencies"),
        }
    }
}

/// Parse the Gomo-relevant fields from a package `gleam.toml`.
pub fn parse_manifest(path: &Path) -> Result<GleamManifest> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read Gleam manifest {}", path.display()))?;
    let manifest = toml::from_str::<RawGleamManifest>(&text)
        .with_context(|| format!("invalid TOML in {}", path.display()))?;

    if manifest.name.trim().is_empty() {
        return Err(anyhow!(
            "{} must define a non-empty `name` string",
            path.display()
        ));
    }

    let mut path_dependencies = Vec::new();
    collect_path_dependencies(
        manifest.dependencies,
        DependencyTable::Dependencies,
        &mut path_dependencies,
    );
    collect_path_dependencies(
        manifest.dev_dependencies_dash,
        DependencyTable::DevDependencies,
        &mut path_dependencies,
    );
    collect_path_dependencies(
        manifest.dev_dependencies_underscore,
        DependencyTable::DevDependencies,
        &mut path_dependencies,
    );

    path_dependencies.sort_by(|left, right| {
        left.table
            .cmp(&right.table)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });

    let gomo = manifest.tools.gomo;

    Ok(GleamManifest {
        name: manifest.name,
        version: normalize_optional_string(manifest.version),
        target: manifest.target.unwrap_or_else(|| "erlang".to_string()),
        path_dependencies,
        gomo_targets: collect_gomo_targets(path, &gomo)?,
        gomo_dev: collect_gomo_dev(path, &gomo)?,
        gomo_tasks: gomo.tasks,
    })
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        if value.is_empty() { None } else { Some(value) }
    })
}

fn collect_gomo_targets(
    path: &Path,
    tools: &RawGomoTools,
) -> Result<BTreeMap<String, GomoTargetConfig>> {
    let mut targets = BTreeMap::new();
    insert_gomo_target(&mut targets, "build", tools.build.clone());
    insert_gomo_target(&mut targets, "format", tools.format.clone());
    insert_gomo_target(&mut targets, "test", tools.test.clone());
    insert_gomo_target(&mut targets, "watch", tools.watch.clone());
    validate_format_command_pair(path, &targets)?;
    validate_target_task_bindings(path, &targets)?;
    validate_cached_folders(path, &targets)?;
    Ok(targets)
}

fn validate_target_task_bindings(
    path: &Path,
    targets: &BTreeMap<String, GomoTargetConfig>,
) -> Result<()> {
    for (target, config) in targets {
        if config.task.is_some() && (config.command.is_some() || config.check_command.is_some()) {
            bail!(
                "{} defines [tools.gomo.{target}].task together with a command; `task` and `command` are mutually exclusive",
                path.display()
            );
        }
        if config.task.is_some() && config.cached_folders.is_some() {
            bail!(
                "{} defines [tools.gomo.{target}].task together with cached_folders; task output caching replaces cached_folders",
                path.display()
            );
        }
        if let Some(task) = &config.task {
            crate::task::validate_name(task).map_err(|error| {
                anyhow!(
                    "{} has invalid [tools.gomo.{target}].task: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn collect_gomo_dev(_path: &Path, tools: &RawGomoTools) -> Result<GomoDevConfig> {
    let Some(config) = tools.dev.as_ref() else {
        return Ok(GomoDevConfig::default());
    };

    let reload = config
        .reload
        .parse()
        .map_err(|error: String| anyhow!("[tools.gomo.dev].{error}"))?;

    Ok(GomoDevConfig {
        command: config.command.clone(),
        reload,
    })
}

fn insert_gomo_target(
    targets: &mut BTreeMap<String, GomoTargetConfig>,
    target: &str,
    config: Option<RawGomoTarget>,
) {
    if let Some(config) = config {
        targets.insert(
            target.to_string(),
            GomoTargetConfig {
                inputs: config.inputs,
                command: config.command,
                check_command: config.check.and_then(|check| check.command),
                cached_folders: config.cached_folders,
                task: config.task,
            },
        );
    }
}

fn validate_cached_folders(
    manifest_path: &Path,
    targets: &BTreeMap<String, GomoTargetConfig>,
) -> Result<()> {
    for (target, config) in targets {
        let Some(folders) = config.cached_folders.as_ref() else {
            continue;
        };
        if target != "build" {
            bail!(
                "{} defines [tools.gomo.{target}].cached_folders, but cached folders are only supported for build tasks",
                manifest_path.display()
            );
        }
        if folders.is_empty() {
            bail!(
                "{} defines an empty [tools.gomo.build].cached_folders list",
                manifest_path.display()
            );
        }

        let mut paths = Vec::with_capacity(folders.len());
        for folder in folders {
            let path = Path::new(folder);
            if folder.trim().is_empty()
                || path.is_absolute()
                || !path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                bail!(
                    "{} has invalid cached folder `{folder}`; paths must be non-empty project-relative directories without `.` or `..`",
                    manifest_path.display()
                );
            }
            if paths.iter().any(|existing: &PathBuf| existing == path) {
                bail!(
                    "{} defines duplicate cached folder `{folder}`",
                    manifest_path.display()
                );
            }
            if let Some(existing) = paths
                .iter()
                .find(|existing| existing.starts_with(path) || path.starts_with(existing))
            {
                bail!(
                    "{} defines overlapping cached folders `{}` and `{folder}`",
                    manifest_path.display(),
                    existing.display()
                );
            }
            paths.push(path.to_path_buf());
        }
    }

    Ok(())
}

fn validate_format_command_pair(
    path: &Path,
    targets: &BTreeMap<String, GomoTargetConfig>,
) -> Result<()> {
    let Some(format) = targets.get("format") else {
        return Ok(());
    };

    match (format.command.as_ref(), format.check_command.as_ref()) {
        (Some(_), Some(_)) | (None, None) => Ok(()),
        (Some(_), None) => bail!(
            "{} defines [tools.gomo.format].command but is missing [tools.gomo.format.check].command",
            path.display()
        ),
        (None, Some(_)) => bail!(
            "{} defines [tools.gomo.format.check].command but is missing [tools.gomo.format].command",
            path.display()
        ),
    }
}

fn collect_path_dependencies(
    dependencies: BTreeMap<String, RawDependency>,
    dependency_table: DependencyTable,
    output: &mut Vec<GleamPathDependency>,
) {
    for (dependency_name, dependency_value) in dependencies {
        match dependency_value {
            RawDependency::Version(_version) => {}
            RawDependency::Inline(dependency) => {
                if let Some(dependency_path) = dependency.path {
                    output.push(GleamPathDependency {
                        name: dependency_name,
                        path: dependency_path,
                        table: dependency_table,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestWorkspace;

    #[test]
    fn parses_name_target_and_path_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"
target = "javascript"

[dependencies]
external = ">= 1.0.0 and < 2.0.0"
local_dep = { path = "../local_dep" }

[dev_dependencies]
local_test_dep = { path = "../local_test_dep" }
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");

        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.version.as_deref(), Some("0.1.0"));
        assert_eq!(manifest.target, "javascript");
        assert_eq!(
            manifest.path_dependencies,
            vec![
                GleamPathDependency {
                    name: "local_dep".to_string(),
                    path: PathBuf::from("../local_dep"),
                    table: DependencyTable::Dependencies,
                },
                GleamPathDependency {
                    name: "local_test_dep".to_string(),
                    path: PathBuf::from("../local_test_dep"),
                    table: DependencyTable::DevDependencies,
                },
            ]
        );
    }

    #[test]
    fn defaults_missing_target_to_erlang() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "default_target"
version = "0.1.0"
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");

        assert_eq!(manifest.target, "erlang");
    }

    #[test]
    fn parses_gomo_target_input_overrides() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.test]
inputs = ["gleam.toml", "src/**", "fixtures/**"]
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");

        assert_eq!(
            manifest
                .gomo_targets
                .get("test")
                .and_then(|config| config.inputs.as_ref()),
            Some(&vec![
                "gleam.toml".to_string(),
                "src/**".to_string(),
                "fixtures/**".to_string(),
            ])
        );
    }

    #[test]
    fn parses_build_cached_folders() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
cached_folders = ["build", "dist"]
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");

        assert_eq!(
            manifest
                .gomo_targets
                .get("build")
                .and_then(|config| config.cached_folders.as_ref()),
            Some(&vec!["build".to_string(), "dist".to_string()])
        );
    }

    #[test]
    fn rejects_invalid_or_overlapping_cached_folders() {
        for folders in [
            "[]",
            r#"["../dist"]"#,
            r#"["dist", "dist"]"#,
            r#"["build", "build/dev"]"#,
        ] {
            let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
            let path = test_workspace.write_file(
                "gleam.toml",
                &format!(
                    r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
cached_folders = {folders}
"#
                ),
            );

            parse_manifest(&path).expect_err("invalid cached folders should fail");
        }
    }

    #[test]
    fn rejects_cached_folders_for_non_build_targets() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.test]
cached_folders = ["coverage"]
"#,
        );

        let error = parse_manifest(&path).expect_err("test cached folders should fail");
        assert!(error.to_string().contains("only supported for build tasks"));
    }

    #[test]
    fn parses_gomo_target_command_overrides() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.test]
command = "gleam test --target erlang"

[tools.gomo.format]
command = "gleam format"

[tools.gomo.format.check]
command = "gleam format --check"
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");

        assert_eq!(
            manifest
                .gomo_targets
                .get("test")
                .and_then(|config| config.command.as_deref()),
            Some("gleam test --target erlang")
        );
        assert_eq!(
            manifest
                .gomo_targets
                .get("format")
                .and_then(|config| config.command.as_deref()),
            Some("gleam format")
        );
        assert_eq!(
            manifest
                .gomo_targets
                .get("format")
                .and_then(|config| config.check_command.as_deref()),
            Some("gleam format --check")
        );
    }

    #[test]
    fn parses_watch_and_dev_configuration() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.watch]
command = "./scripts/generate.sh"

[tools.gomo.dev]
command = "gleam run -m demo"
reload = "hot"
"#,
        );

        let manifest = parse_manifest(&path).expect("manifest should parse");
        assert_eq!(
            manifest
                .gomo_targets
                .get("watch")
                .and_then(|config| config.command.as_deref()),
            Some("./scripts/generate.sh")
        );
        assert_eq!(
            manifest.gomo_dev.command.as_deref(),
            Some("gleam run -m demo")
        );
        assert_eq!(manifest.gomo_dev.reload, GomoReloadStrategy::Hot);
    }

    #[test]
    fn rejects_unknown_reload_strategy() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.dev]
reload = "maybe"
"#,
        );

        let error = parse_manifest(&path).expect_err("unknown reload should fail");
        assert!(
            error
                .to_string()
                .contains("reload must be `restart` or `hot`")
        );
    }

    #[test]
    fn rejects_custom_format_without_format_check() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.format]
command = "gleam format"
"#,
        );

        let error = parse_manifest(&path).expect_err("partial format command should fail");

        assert!(
            error
                .to_string()
                .contains("missing [tools.gomo.format.check].command")
        );
    }

    #[test]
    fn rejects_custom_format_check_without_format() {
        let test_workspace = TestWorkspace::new("gomo-gleam-toml-test");
        let path = test_workspace.write_file(
            "gleam.toml",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.format.check]
command = "gleam format --check"
"#,
        );

        let error = parse_manifest(&path).expect_err("partial format command should fail");

        assert!(
            error
                .to_string()
                .contains("missing [tools.gomo.format].command")
        );
    }
}
