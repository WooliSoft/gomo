use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::commands::{CommandOutput, OutputOptions};
use crate::dependency_versions;
use crate::graph::ProjectGraph;
use crate::workspace::{self, Workspace};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DoctorReport {
    status: String,
    workspace_root: Option<String>,
    cache_dir: Option<String>,
    project_count: usize,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DoctorCheck {
    name: String,
    status: String,
    message: String,
}

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let report = diagnose(cwd);
    let exit_code = if report.has_errors() { 1 } else { 0 };
    let output = if output_options.json {
        render_json(&report)?
    } else {
        render_human(&report)
    };

    Ok(CommandOutput::with_exit_code(output, exit_code))
}

fn diagnose(cwd: &Path) -> DoctorReport {
    let mut report = DoctorReport {
        status: "ok".to_string(),
        workspace_root: None,
        cache_dir: None,
        project_count: 0,
        checks: Vec::new(),
    };

    let workspace = match workspace::discover_from(cwd) {
        Ok(workspace) => workspace,
        Err(error) => {
            report.error(
                "workspace discovery",
                format!("{error}. Run from a Gomo workspace or add gomo.toml at the repo root."),
            );
            return report;
        }
    };

    report.workspace_root = Some(workspace.root.display().to_string());
    report.cache_dir = Some(workspace.cache_dir.display().to_string());
    report.project_count = workspace.projects.len();
    report.ok(
        "workspace discovery",
        format!("found workspace root {}", workspace.root.display()),
    );

    check_projects(&mut report, &workspace);
    check_cache_dir(&mut report, &workspace);
    check_graph(&mut report, &workspace);
    check_dependency_versions(&mut report, &workspace);
    report.finish();

    report
}

fn check_dependency_versions(report: &mut DoctorReport, workspace: &Workspace) {
    if !workspace.dependency_versions.enabled {
        return;
    }

    let dependency_report =
        dependency_versions::check_workspace(workspace, &workspace.dependency_versions);
    if dependency_report.is_success() {
        report.ok(
            "dependency versions",
            format!(
                "validated resolved dependency versions from {} manifest(s)",
                dependency_report.checked_manifest_count
            ),
        );
    } else {
        report.error(
            "dependency versions",
            format!(
                "found {} dependency version issue(s). Run `gomo deps check` for details.",
                dependency_report.issue_count()
            ),
        );
    }
}

fn check_projects(report: &mut DoctorReport, workspace: &Workspace) {
    if workspace.projects.is_empty() {
        report.warning(
            "project discovery",
            format!(
                "no Gleam projects found under {}. Add gleam.toml under those roots or update the workspace layout.",
                workspace.project_globs.join(", ")
            ),
        );
    } else {
        report.ok(
            "project discovery",
            format!("found {} Gleam project(s)", workspace.projects.len()),
        );
    }
}

fn check_cache_dir(report: &mut DoctorReport, workspace: &Workspace) {
    match fs::symlink_metadata(&workspace.cache_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => report.ok(
            "cache directory",
            format!("cache directory exists at {}", workspace.cache_dir.display()),
        ),
        Ok(_) => report.error(
            "cache directory",
            format!(
                "configured cache path exists but is not a directory: {}. Move it or update [cache].dir.",
                workspace.cache_dir.display()
            ),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.ok(
            "cache directory",
            format!(
                "cache directory can be created at {}",
                workspace.cache_dir.display()
            ),
        ),
        Err(error) => report.error(
            "cache directory",
            format!("failed to inspect {}: {error}", workspace.cache_dir.display()),
        ),
    }
}

fn check_graph(report: &mut DoctorReport, workspace: &Workspace) {
    match ProjectGraph::build(workspace) {
        Ok(graph) => {
            report.ok(
                "dependency graph",
                format!(
                    "validated {} project(s) in dependency order",
                    graph.topological_order.len()
                ),
            );
            let unmanaged = graph
                .unmanaged_path_dependencies
                .values()
                .map(Vec::len)
                .sum::<usize>();
            if unmanaged > 0 {
                report.warning(
                    "unmanaged path dependencies",
                    format!(
                        "found {unmanaged} local path dependency entries inside the workspace but outside discovered project roots. Add those packages to the Gomo workspace layout if they should be scheduled."
                    ),
                );
            }
        }
        Err(error) => report.error(
            "dependency graph",
            format!("{error}. Check local path dependencies in gleam.toml and resolve any cycles."),
        ),
    }
}

fn render_human(report: &DoctorReport) -> String {
    let mut output = String::new();
    output.push_str("Doctor\n");
    output.push_str(&format!("Status: {}\n", report.status));
    if let Some(workspace_root) = &report.workspace_root {
        output.push_str(&format!("Workspace Root: {workspace_root}\n"));
    }
    if let Some(cache_dir) = &report.cache_dir {
        output.push_str(&format!("Cache Dir: {cache_dir}\n"));
    }
    output.push_str(&format!("Projects: {}\n", report.project_count));

    for check in &report.checks {
        output.push_str(&format!(
            "[{}] {}: {}\n",
            check.status, check.name, check.message
        ));
    }

    output
}

fn render_json(report: &DoctorReport) -> Result<String> {
    let mut json =
        serde_json::to_string_pretty(report).context("failed to serialize doctor JSON")?;
    json.push('\n');
    Ok(json)
}

impl DoctorReport {
    fn ok(&mut self, name: impl Into<String>, message: impl Into<String>) {
        self.checks.push(DoctorCheck {
            name: name.into(),
            status: "ok".to_string(),
            message: message.into(),
        });
    }

    fn warning(&mut self, name: impl Into<String>, message: impl Into<String>) {
        self.checks.push(DoctorCheck {
            name: name.into(),
            status: "warning".to_string(),
            message: message.into(),
        });
    }

    fn error(&mut self, name: impl Into<String>, message: impl Into<String>) {
        self.checks.push(DoctorCheck {
            name: name.into(),
            status: "error".to_string(),
            message: message.into(),
        });
    }

    fn has_errors(&self) -> bool {
        self.checks.iter().any(|check| check.status == "error")
    }

    fn finish(&mut self) {
        if self.has_errors() {
            self.status = "error".to_string();
        } else if self.checks.iter().any(|check| check.status == "warning") {
            self.status = "warning".to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn doctor_reports_healthy_workspace() {
        let test_workspace = TestWorkspace::new("gomo-doctor-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("doctor should inspect workspace");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Doctor"));
        assert!(output.stdout.contains("Status: ok"));
        assert!(output.stdout.contains("[ok] dependency graph"));
    }

    #[test]
    fn doctor_reports_missing_workspace_as_actionable_error() {
        let test_workspace = TestWorkspace::new("gomo-doctor-command-test");

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("doctor should render discovery errors");

        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.contains("[error] workspace discovery"));
        assert!(output.stdout.contains("add gomo.toml"));
    }

    #[test]
    fn doctor_renders_json() {
        let test_workspace = TestWorkspace::new("gomo-doctor-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let output = run(
            test_workspace.path(),
            OutputOptions {
                json: true,
                ci: true,
                tui: false,
            },
        )
        .expect("doctor JSON should render");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert_eq!(value["status"], "ok");
        assert_eq!(value["project_count"], 1);
    }

    #[test]
    fn doctor_runs_dependency_version_check_when_enabled() {
        let test_workspace = TestWorkspace::new("gomo-doctor-command-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[dependency_versions]
enabled = true
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("doctor should inspect dependency versions");

        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.contains("[error] dependency versions"));
        assert!(output.stdout.contains("gomo deps check"));
    }
}
