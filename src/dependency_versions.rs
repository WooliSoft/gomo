use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::gleam_lock::{LockedPackage, LockedPackageSource, parse_lock_manifest};
use crate::gleam_toml::parse_manifest;
use crate::workspace::{DependencyVersionConfig, Project, Workspace};

/// Result of checking resolved dependency versions across Gleam lock manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DependencyVersionReport {
    pub(crate) status: String,
    pub(crate) project_count: usize,
    pub(crate) checked_manifest_count: usize,
    pub(crate) missing_manifests: Vec<MissingManifest>,
    pub(crate) manifest_errors: Vec<ManifestError>,
    pub(crate) version_mismatches: Vec<VersionMismatch>,
    pub(crate) local_version_mismatches: Vec<LocalVersionMismatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MissingManifest {
    pub(crate) project: String,
    pub(crate) manifest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ManifestError {
    pub(crate) project: String,
    pub(crate) manifest: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VersionMismatch {
    pub(crate) dependency: String,
    pub(crate) source: String,
    pub(crate) versions: Vec<VersionGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VersionGroup {
    pub(crate) version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
    pub(crate) occurrences: Vec<DependencyOccurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DependencyOccurrence {
    pub(crate) project: String,
    pub(crate) manifest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LocalVersionMismatch {
    pub(crate) dependency: String,
    pub(crate) project: String,
    pub(crate) manifest: String,
    pub(crate) locked_version: String,
    pub(crate) local_path: String,
    pub(crate) declared_manifest: Option<String>,
    pub(crate) declared_version: Option<String>,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DependencyKey {
    name: String,
    source: LockedPackageSource,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolutionKey {
    version: String,
    repo: Option<String>,
    commit: Option<String>,
}

/// Check resolved package versions across all discovered workspace projects.
pub(crate) fn check_workspace(
    workspace: &Workspace,
    config: &DependencyVersionConfig,
) -> DependencyVersionReport {
    let ignored = config.ignore.iter().cloned().collect::<BTreeSet<_>>();
    let mut report = DependencyVersionReport {
        status: "ok".to_string(),
        project_count: workspace.projects.len(),
        checked_manifest_count: 0,
        missing_manifests: Vec::new(),
        manifest_errors: Vec::new(),
        version_mismatches: Vec::new(),
        local_version_mismatches: Vec::new(),
    };
    let mut grouped_versions =
        BTreeMap::<DependencyKey, BTreeMap<ResolutionKey, Vec<DependencyOccurrence>>>::new();

    for project in &workspace.projects {
        let lock_manifest_path = project.root.join("manifest.toml");
        let lock_manifest_display = workspace_relative_path(workspace, &lock_manifest_path);
        if !lock_manifest_path.is_file() {
            report.missing_manifests.push(MissingManifest {
                project: project.name.clone(),
                manifest: lock_manifest_display,
            });
            continue;
        }

        let lock_manifest = match parse_lock_manifest(&lock_manifest_path) {
            Ok(lock_manifest) => lock_manifest,
            Err(error) => {
                report.manifest_errors.push(ManifestError {
                    project: project.name.clone(),
                    manifest: lock_manifest_display,
                    message: error.to_string(),
                });
                continue;
            }
        };
        report.checked_manifest_count += 1;

        for package in lock_manifest.packages {
            if ignored.contains(&package.name) || should_skip_package(&package, config) {
                continue;
            }

            let is_git = is_git_package(&package);
            grouped_versions
                .entry(DependencyKey {
                    name: package.name.clone(),
                    source: package.source.clone(),
                })
                .or_default()
                .entry(ResolutionKey {
                    version: package.version.clone(),
                    repo: if is_git { package.repo.clone() } else { None },
                    commit: if is_git { package.commit.clone() } else { None },
                })
                .or_default()
                .push(DependencyOccurrence {
                    project: project.name.clone(),
                    manifest: lock_manifest_display.clone(),
                });

            if package.source == LockedPackageSource::Local {
                check_local_package(
                    workspace,
                    project,
                    &lock_manifest_display,
                    &package,
                    &mut report,
                );
            }
        }
    }

    for (key, versions) in grouped_versions {
        if versions.len() <= 1 {
            continue;
        }
        report.version_mismatches.push(VersionMismatch {
            dependency: key.name,
            source: key.source.to_string(),
            versions: versions
                .into_iter()
                .map(|(resolution, mut occurrences)| {
                    occurrences.sort_by(|left, right| left.project.cmp(&right.project));
                    VersionGroup {
                        version: resolution.version,
                        repo: resolution.repo,
                        commit: resolution.commit,
                        occurrences,
                    }
                })
                .collect(),
        });
    }

    report.finish();
    report
}

fn is_git_package(package: &LockedPackage) -> bool {
    package.source.as_str() == "git"
}

fn should_skip_package(package: &LockedPackage, config: &DependencyVersionConfig) -> bool {
    package.source == LockedPackageSource::Local && !config.include_local
}

fn check_local_package(
    workspace: &Workspace,
    project: &Project,
    lock_manifest_display: &str,
    package: &LockedPackage,
    report: &mut DependencyVersionReport,
) {
    let Some(local_path) = &package.path else {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: "".to_string(),
            declared_manifest: None,
            declared_version: None,
            message: "local package entry is missing `path`".to_string(),
        });
        return;
    };

    let local_path_display = local_path.display().to_string();
    let local_root = match project.root.join(local_path).canonicalize() {
        Ok(local_root) => local_root,
        Err(error) => {
            report.local_version_mismatches.push(LocalVersionMismatch {
                dependency: package.name.clone(),
                project: project.name.clone(),
                manifest: lock_manifest_display.to_string(),
                locked_version: package.version.clone(),
                local_path: local_path_display,
                declared_manifest: None,
                declared_version: None,
                message: format!("failed to resolve local package path: {error}"),
            });
            return;
        }
    };

    if !local_root.starts_with(&workspace.root) {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: local_path_display,
            declared_manifest: None,
            declared_version: None,
            message: format!(
                "local package path resolves outside the workspace: {}",
                local_root.display()
            ),
        });
        return;
    }

    let local_manifest_path = local_root.join("gleam.toml");
    let local_manifest_display = workspace_relative_path(workspace, &local_manifest_path);
    if !local_manifest_path.is_file() {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: local_path_display,
            declared_manifest: Some(local_manifest_display),
            declared_version: None,
            message: "local package path does not contain gleam.toml".to_string(),
        });
        return;
    }

    let local_manifest = match parse_manifest(&local_manifest_path) {
        Ok(local_manifest) => local_manifest,
        Err(error) => {
            report.local_version_mismatches.push(LocalVersionMismatch {
                dependency: package.name.clone(),
                project: project.name.clone(),
                manifest: lock_manifest_display.to_string(),
                locked_version: package.version.clone(),
                local_path: local_path_display,
                declared_manifest: Some(local_manifest_display),
                declared_version: None,
                message: format!("failed to parse local package manifest: {error}"),
            });
            return;
        }
    };

    if local_manifest.name != package.name {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: local_path_display,
            declared_manifest: Some(local_manifest_display),
            declared_version: local_manifest.version,
            message: format!(
                "local package manifest declares `{}` instead of `{}`",
                local_manifest.name, package.name
            ),
        });
        return;
    }

    let Some(declared_version) = local_manifest.version else {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: local_path_display,
            declared_manifest: Some(local_manifest_display),
            declared_version: None,
            message: "local package manifest is missing `version`".to_string(),
        });
        return;
    };

    if declared_version != package.version {
        report.local_version_mismatches.push(LocalVersionMismatch {
            dependency: package.name.clone(),
            project: project.name.clone(),
            manifest: lock_manifest_display.to_string(),
            locked_version: package.version.clone(),
            local_path: local_path_display,
            declared_manifest: Some(local_manifest_display),
            declared_version: Some(declared_version.clone()),
            message: format!(
                "local package is locked as {} but declares {}",
                package.version, declared_version
            ),
        });
    }
}

fn workspace_relative_path(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(&workspace.root)
        .unwrap_or(path)
        .display()
        .to_string()
}

impl DependencyVersionReport {
    pub(crate) fn is_success(&self) -> bool {
        self.status == "ok"
    }

    pub(crate) fn issue_count(&self) -> usize {
        self.missing_manifests.len()
            + self.manifest_errors.len()
            + self.version_mismatches.len()
            + self.local_version_mismatches.len()
    }

    fn finish(&mut self) {
        if self.issue_count() > 0 {
            self.status = "error".to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestWorkspace;

    #[test]
    fn accepts_matching_resolved_versions_with_different_ranges() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "one"
version = "0.1.0"

[dependencies]
lustre = ">= 5.6.0 and < 6.0.0"
"#,
        );
        test_workspace.write_manifest(
            "apps/two",
            r#"
name = "two"
version = "0.1.0"

[dependencies]
lustre = ">= 5.7.0 and < 6.0.0"
"#,
        );
        test_workspace.write_file(
            "apps/one/manifest.toml",
            r#"
packages = [
  { name = "lustre", version = "5.7.0", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        test_workspace.write_file(
            "apps/two/manifest.toml",
            r#"
packages = [
  { name = "lustre", version = "5.7.0", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert!(report.is_success());
        assert!(report.version_mismatches.is_empty());
    }

    #[test]
    fn reports_resolved_version_mismatches() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "one"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "apps/two",
            r#"
name = "two"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "apps/one/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        test_workspace.write_file(
            "apps/two/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.3", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert_eq!(report.status, "error");
        assert_eq!(report.version_mismatches.len(), 1);
        assert_eq!(report.version_mismatches[0].dependency, "gleam_stdlib");
        assert_eq!(report.version_mismatches[0].source, "hex");
    }

    #[test]
    fn accepts_matching_git_resolutions() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-git-test");
        test_workspace.write_gomo_config();
        for project in ["one", "two"] {
            test_workspace.write_manifest(
                &format!("apps/{project}"),
                &format!("name = \"{project}\"\nversion = \"0.1.0\"\n"),
            );
            test_workspace.write_file(
                &format!("apps/{project}/manifest.toml"),
                r#"
packages = [
  { name = "lustre", version = "5.7.0", source = "git", repo = "https://github.com/lustre-labs/lustre", commit = "abc123" },
]
"#,
            );
        }
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert!(report.is_success());
        assert!(report.version_mismatches.is_empty());
    }

    #[test]
    fn reports_mismatched_git_commits_and_repositories() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-git-test");
        test_workspace.write_gomo_config();
        let resolutions = [
            ("one", "https://github.com/lustre-labs/lustre", "abc123"),
            ("two", "https://github.com/lustre-labs/lustre", "def456"),
            ("three", "https://github.com/example/lustre", "abc123"),
        ];
        for (project, repo, commit) in resolutions {
            test_workspace.write_manifest(
                &format!("apps/{project}"),
                &format!("name = \"{project}\"\nversion = \"0.1.0\"\n"),
            );
            test_workspace.write_file(
                &format!("apps/{project}/manifest.toml"),
                &format!(
                    "packages = [\n  {{ name = \"lustre\", version = \"5.7.0\", source = \"git\", repo = \"{repo}\", commit = \"{commit}\" }},\n]\n"
                ),
            );
        }
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert_eq!(report.version_mismatches.len(), 1);
        let mismatch = &report.version_mismatches[0];
        assert_eq!(mismatch.dependency, "lustre");
        assert_eq!(mismatch.source, "git");
        assert_eq!(mismatch.versions.len(), 3);
        assert!(mismatch.versions.iter().any(|resolution| {
            resolution.repo.as_deref() == Some("https://github.com/lustre-labs/lustre")
                && resolution.commit.as_deref() == Some("def456")
        }));
        assert!(mismatch.versions.iter().any(|resolution| {
            resolution.repo.as_deref() == Some("https://github.com/example/lustre")
                && resolution.commit.as_deref() == Some("abc123")
        }));
    }

    #[test]
    fn checks_local_versions_against_declared_package_versions() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
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
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.2.0"
"#,
        );
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            r#"
packages = [
  { name = "shared", version = "0.1.0", build_tools = ["gleam"], requirements = [], source = "local", path = "../../libs/shared" },
]
"#,
        );
        test_workspace.write_file(
            "libs/shared/manifest.toml",
            r#"
packages = []
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert_eq!(report.status, "error");
        assert_eq!(report.local_version_mismatches.len(), 1);
        assert_eq!(report.local_version_mismatches[0].dependency, "shared");
        assert_eq!(
            report.local_version_mismatches[0]
                .declared_version
                .as_deref(),
            Some("0.2.0")
        );
    }

    #[test]
    fn can_skip_local_packages() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*", "libs/*"]

[dependency_versions]
include_local = false
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
shared = { path = "../../libs/shared" }
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.2.0"
"#,
        );
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            r#"
packages = [
  { name = "shared", version = "0.1.0", build_tools = ["gleam"], requirements = [], source = "local", path = "../../libs/shared" },
]
"#,
        );
        test_workspace.write_file(
            "libs/shared/manifest.toml",
            r#"
packages = []
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert!(report.is_success());
        assert!(report.local_version_mismatches.is_empty());
    }

    #[test]
    fn reports_missing_lock_manifests() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert_eq!(report.status, "error");
        assert_eq!(report.missing_manifests.len(), 1);
        assert_eq!(
            report.missing_manifests[0].manifest,
            "apps/demo/manifest.toml"
        );
    }

    #[test]
    fn ignores_configured_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-dependency-versions-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[dependency_versions]
ignore = ["gleam_stdlib"]
"#,
        );
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "one"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "apps/two",
            r#"
name = "two"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "apps/one/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        test_workspace.write_file(
            "apps/two/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.3", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );
        let workspace = crate::workspace::discover(test_workspace.path())
            .expect("workspace should be discovered");

        let report = check_workspace(&workspace, &workspace.dependency_versions);

        assert!(report.is_success());
        assert!(report.version_mismatches.is_empty());
    }
}
