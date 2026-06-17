use std::path::Path;

use anyhow::{Context, Result};

use crate::commands::{CommandOutput, OutputOptions};
use crate::dependency_versions::{self, DependencyVersionReport};
use crate::workspace;

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let report = dependency_versions::check_workspace(&workspace, &workspace.dependency_versions);
    let exit_code = if report.is_success() { 0 } else { 1 };
    let output = if output_options.json {
        render_json(&report)?
    } else {
        render_human(&report)
    };

    Ok(CommandOutput::with_exit_code(output, exit_code))
}

fn render_json(report: &DependencyVersionReport) -> Result<String> {
    let mut json = serde_json::to_string_pretty(report)
        .context("failed to serialize dependency versions JSON")?;
    json.push('\n');
    Ok(json)
}

fn render_human(report: &DependencyVersionReport) -> String {
    let mut output = String::new();
    output.push_str("Dependency Versions\n");
    output.push_str(&format!("Status: {}\n", report.status));
    output.push_str(&format!("Projects: {}\n", report.project_count));
    output.push_str(&format!("Manifests: {}\n", report.checked_manifest_count));

    if report.is_success() {
        output.push_str("[ok] all checked dependencies resolve to one version\n");
        return output;
    }

    if !report.missing_manifests.is_empty() {
        output.push_str("[error] missing manifest.toml files:\n");
        for missing in &report.missing_manifests {
            output.push_str(&format!("  - {}: {}\n", missing.project, missing.manifest));
        }
    }

    if !report.manifest_errors.is_empty() {
        output.push_str("[error] invalid manifest.toml files:\n");
        for error in &report.manifest_errors {
            output.push_str(&format!(
                "  - {}: {} ({})\n",
                error.project, error.manifest, error.message
            ));
        }
    }

    for mismatch in &report.version_mismatches {
        output.push_str(&format!(
            "[error] {} ({}) has multiple resolved versions:\n",
            mismatch.dependency, mismatch.source
        ));
        for version in &mismatch.versions {
            let projects = version
                .occurrences
                .iter()
                .map(|occurrence| occurrence.project.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!("  {}: {}\n", version.version, projects));
        }
    }

    if !report.local_version_mismatches.is_empty() {
        output.push_str("[error] local package lock entries are stale or invalid:\n");
        for mismatch in &report.local_version_mismatches {
            output.push_str(&format!(
                "  - {} in {} locks {} from {}: {}\n",
                mismatch.dependency,
                mismatch.manifest,
                mismatch.locked_version,
                mismatch.local_path,
                mismatch.message
            ));
            if let Some(declared_manifest) = &mismatch.declared_manifest {
                output.push_str(&format!("    declared manifest: {declared_manifest}\n"));
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestWorkspace;

    #[test]
    fn deps_check_reports_success() {
        let test_workspace = TestWorkspace::new("gomo-deps-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("deps check should inspect manifests");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Dependency Versions"));
        assert!(output.stdout.contains("Status: ok"));
    }

    #[test]
    fn deps_check_reports_version_mismatches() {
        let test_workspace = TestWorkspace::new("gomo-deps-command-test");
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

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("deps check should inspect manifests");

        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.contains("gleam_stdlib"));
        assert!(output.stdout.contains("1.0.2: one"));
        assert!(output.stdout.contains("1.0.3: two"));
    }

    #[test]
    fn deps_check_renders_json() {
        let test_workspace = TestWorkspace::new("gomo-deps-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], source = "hex" },
]
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
        .expect("deps check JSON should render");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert_eq!(output.exit_code, 0);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["checked_manifest_count"], 1);
    }
}
