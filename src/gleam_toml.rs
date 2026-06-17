use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

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

#[derive(Debug, Default, Deserialize)]
struct RawGomoTools {
    #[serde(default)]
    build: Option<RawGomoTarget>,
    #[serde(default)]
    format: Option<RawGomoTarget>,
    #[serde(default)]
    test: Option<RawGomoTarget>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawGomoTarget {
    inputs: Option<Vec<String>>,
    command: Option<String>,
    check: Option<RawGomoTargetCheck>,
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

    Ok(GleamManifest {
        name: manifest.name,
        version: normalize_optional_string(manifest.version),
        target: manifest.target.unwrap_or_else(|| "erlang".to_string()),
        path_dependencies,
        gomo_targets: collect_gomo_targets(path, manifest.tools.gomo)?,
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
    tools: RawGomoTools,
) -> Result<BTreeMap<String, GomoTargetConfig>> {
    let mut targets = BTreeMap::new();
    insert_gomo_target(&mut targets, "build", tools.build);
    insert_gomo_target(&mut targets, "format", tools.format);
    insert_gomo_target(&mut targets, "test", tools.test);
    validate_format_command_pair(path, &targets)?;
    Ok(targets)
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
            },
        );
    }
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
