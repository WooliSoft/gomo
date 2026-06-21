use std::path::Path;

use anyhow::{Context, Result};

use crate::commands::{CommandOutput, OutputOptions};
use crate::dependency_versions::{self, DependencyVersionReport};
use crate::workspace;

const DEFAULT_RICH_WIDTH: usize = 100;
const RESET: &str = "\x1b[0m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const BOLD_RED: &str = "\x1b[1;31m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const DIM_GRAY: &str = "\x1b[2;90m";

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let report = dependency_versions::check_workspace(&workspace, &workspace.dependency_versions);
    let exit_code = if report.is_success() { 0 } else { 1 };
    let output = if output_options.json {
        render_json(&report)?
    } else if output_options.ci {
        render_plain(&report)
    } else {
        render_rich(&report, output_options.terminal_width)
    };

    Ok(CommandOutput::with_exit_code(output, exit_code))
}

fn render_json(report: &DependencyVersionReport) -> Result<String> {
    let mut json = serde_json::to_string_pretty(report)
        .context("failed to serialize dependency versions JSON")?;
    json.push('\n');
    Ok(json)
}

fn render_plain(report: &DependencyVersionReport) -> String {
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

fn render_rich(report: &DependencyVersionReport, terminal_width: Option<u16>) -> String {
    let width = terminal_width
        .map(usize::from)
        .unwrap_or(DEFAULT_RICH_WIDTH)
        .max(1);
    let mut output = String::new();

    push_border_line(&mut output, '╭', '─', '╮', width);
    push_bordered_line(&mut output, "Dependency Versions", width, Some(BOLD_CYAN));
    push_separator_line(&mut output, width);
    push_bordered_line(
        &mut output,
        &format!("Status: {}", report.status),
        width,
        Some(if report.is_success() {
            BOLD_GREEN
        } else {
            BOLD_RED
        }),
    );
    push_bordered_line(
        &mut output,
        &format!("Projects: {}", report.project_count),
        width,
        None,
    );
    push_bordered_line(
        &mut output,
        &format!("Manifests: {} checked", report.checked_manifest_count),
        width,
        None,
    );
    push_bordered_line(
        &mut output,
        &format!("Issues: {}", report.issue_count()),
        width,
        None,
    );

    push_separator_line(&mut output, width);
    if report.is_success() {
        push_bordered_line(
            &mut output,
            "[ok] all checked dependencies resolve to one version",
            width,
            Some(BOLD_GREEN),
        );
        push_border_line(&mut output, '╰', '─', '╯', width);
        return output;
    }

    if !report.missing_manifests.is_empty() {
        push_bordered_line(
            &mut output,
            "[error] missing manifest.toml files:",
            width,
            Some(BOLD_RED),
        );
        for missing in &report.missing_manifests {
            push_bordered_line(
                &mut output,
                &format!("  - {}: {}", missing.project, missing.manifest),
                width,
                None,
            );
        }
    }

    if !report.manifest_errors.is_empty() {
        push_bordered_line(
            &mut output,
            "[error] invalid manifest.toml files:",
            width,
            Some(BOLD_RED),
        );
        for error in &report.manifest_errors {
            push_bordered_line(
                &mut output,
                &format!(
                    "  - {}: {} ({})",
                    error.project, error.manifest, error.message
                ),
                width,
                None,
            );
        }
    }

    for mismatch in &report.version_mismatches {
        push_bordered_line(
            &mut output,
            &format!(
                "[error] {} ({}) has multiple resolved versions:",
                mismatch.dependency, mismatch.source
            ),
            width,
            Some(BOLD_RED),
        );
        for version in &mismatch.versions {
            let projects = version
                .occurrences
                .iter()
                .map(|occurrence| occurrence.project.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            push_bordered_line(
                &mut output,
                &format!("  {}: {}", version.version, projects),
                width,
                Some(BOLD_YELLOW),
            );
        }
    }

    if !report.local_version_mismatches.is_empty() {
        push_bordered_line(
            &mut output,
            "[error] local package lock entries are stale or invalid:",
            width,
            Some(BOLD_RED),
        );
        for mismatch in &report.local_version_mismatches {
            push_bordered_line(
                &mut output,
                &format!(
                    "  - {} in {} locks {} from {}: {}",
                    mismatch.dependency,
                    mismatch.manifest,
                    mismatch.locked_version,
                    mismatch.local_path,
                    mismatch.message
                ),
                width,
                None,
            );
            if let Some(declared_manifest) = &mismatch.declared_manifest {
                push_bordered_line(
                    &mut output,
                    &format!("    declared manifest: {declared_manifest}"),
                    width,
                    None,
                );
            }
        }
    }

    push_border_line(&mut output, '╰', '─', '╯', width);
    output
}

fn content_width(width: usize) -> usize {
    width.saturating_sub(4)
}

fn push_border_line(output: &mut String, left: char, fill: char, right: char, width: usize) {
    if width < 2 {
        output.push_str(&truncate(&left.to_string(), width));
        output.push('\n');
        return;
    }

    output.push_str(DIM_GRAY);
    output.push(left);
    output.push_str(&fill.to_string().repeat(width.saturating_sub(2)));
    output.push(right);
    output.push_str(RESET);
    output.push('\n');
}

fn push_separator_line(output: &mut String, width: usize) {
    push_border_line(output, '├', '─', '┤', width);
}

fn push_bordered_line(output: &mut String, text: &str, width: usize, style: Option<&str>) {
    if width < 4 {
        output.push_str(&truncate(text, width));
        output.push('\n');
        return;
    }

    let content_width = content_width(width);
    let content = truncate(text, content_width);
    let padding = content_width.saturating_sub(content.chars().count());

    output.push_str(DIM_GRAY);
    output.push('│');
    output.push(' ');
    output.push_str(RESET);
    if let Some(style) = style {
        output.push_str(style);
        output.push_str(&content);
        output.push_str(RESET);
    } else {
        output.push_str(&content);
    }
    output.push_str(&" ".repeat(padding));
    output.push_str(DIM_GRAY);
    output.push(' ');
    output.push('│');
    output.push_str(RESET);
    output.push('\n');
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }

    if width <= 3 {
        return text.chars().take(width).collect();
    }

    let mut truncated = text.chars().take(width - 3).collect::<String>();
    truncated.push_str("...");
    truncated
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
        let visible = strip_ansi(&output.stdout);

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Dependency Versions"));
        assert!(output.stdout.contains("\x1b[1;36m"));
        assert!(output.stdout.contains("\x1b[1;32m"));
        assert!(visible.contains("╭"));
        assert!(visible.contains("Status: ok"));
        assert!(visible.contains("[ok] all checked dependencies resolve to one version"));
        assert_lines_fit(&output.stdout, DEFAULT_RICH_WIDTH);
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
        let visible = strip_ansi(&output.stdout);

        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.contains("\x1b[1;31m"));
        assert!(output.stdout.contains("\x1b[1;33m"));
        assert!(visible.contains("Status: error"));
        assert!(visible.contains("gleam_stdlib"));
        assert!(visible.contains("1.0.2: one"));
        assert!(visible.contains("1.0.3: two"));
        assert_lines_fit(&output.stdout, DEFAULT_RICH_WIDTH);
    }

    #[test]
    fn deps_check_ci_output_stays_plain() {
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
                json: false,
                ci: true,
                tui: false,
                terminal_width: None,
            },
        )
        .expect("deps check should render plain CI output");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Dependency Versions"));
        assert!(output.stdout.contains("Status: ok"));
        assert!(!output.stdout.contains("\x1b["));
        assert!(!output.stdout.contains("╭"));
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
                terminal_width: None,
            },
        )
        .expect("deps check JSON should render");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert_eq!(output.exit_code, 0);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["checked_manifest_count"], 1);
    }

    fn assert_lines_fit(output: &str, width: usize) {
        for line in output.lines() {
            assert!(
                visible_width(line) <= width,
                "line should fit {width} columns: {line:?}"
            );
        }
    }

    fn strip_ansi(text: &str) -> String {
        text.lines()
            .map(strip_ansi_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn visible_width(line: &str) -> usize {
        strip_ansi_line(line).chars().count()
    }

    fn strip_ansi_line(line: &str) -> String {
        let mut stripped = String::new();
        let mut chars = line.chars();

        while let Some(char) = chars.next() {
            if char == '\x1b' {
                for escaped in chars.by_ref() {
                    if escaped == 'm' {
                        break;
                    }
                }
            } else {
                stripped.push(char);
            }
        }

        stripped
    }
}
