use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::commands::{CommandOutput, OutputOptions};
use crate::dependency_versions;
use crate::graph::ProjectGraph;
use crate::workspace::{self, Workspace};

const DEFAULT_RICH_WIDTH: usize = 100;
const RESET: &str = "\x1b[0m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const BOLD_RED: &str = "\x1b[1;31m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const DIM_GRAY: &str = "\x1b[2;90m";

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
    } else if output_options.ci {
        render_plain(&report)
    } else {
        render_rich(&report, output_options.terminal_width)
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

fn render_plain(report: &DoctorReport) -> String {
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

fn render_rich(report: &DoctorReport, terminal_width: Option<u16>) -> String {
    let width = terminal_width
        .map(usize::from)
        .unwrap_or(DEFAULT_RICH_WIDTH)
        .max(1);
    let mut output = String::new();

    push_border_line(&mut output, '╭', '─', '╮', width);
    push_bordered_line(&mut output, "Doctor", width, Some(BOLD_CYAN));
    push_separator_line(&mut output, width);
    push_bordered_line(
        &mut output,
        &format!("Status: {}", report.status),
        width,
        Some(status_style(&report.status)),
    );
    if let Some(workspace_root) = &report.workspace_root {
        push_wrapped_bordered_line(
            &mut output,
            &format!("Workspace Root: {workspace_root}"),
            width,
            None,
        );
    }
    if let Some(cache_dir) = &report.cache_dir {
        push_wrapped_bordered_line(&mut output, &format!("Cache Dir: {cache_dir}"), width, None);
    }
    push_bordered_line(
        &mut output,
        &format!("Projects: {}", report.project_count),
        width,
        None,
    );

    if !report.checks.is_empty() {
        push_separator_line(&mut output, width);
    }

    for check in &report.checks {
        push_wrapped_bordered_line(
            &mut output,
            &format!("[{}] {}: {}", check.status, check.name, check.message),
            width,
            Some(status_style(&check.status)),
        );
    }

    push_border_line(&mut output, '╰', '─', '╯', width);
    output
}

fn status_style(status: &str) -> &'static str {
    match status {
        "ok" => BOLD_GREEN,
        "warning" => BOLD_YELLOW,
        "error" => BOLD_RED,
        _ => BOLD_CYAN,
    }
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

fn push_wrapped_bordered_line(output: &mut String, text: &str, width: usize, style: Option<&str>) {
    if width < 4 {
        push_bordered_line(output, text, width, style);
        return;
    }

    for line in wrap_text(text, content_width(width)) {
        push_bordered_line(output, &line, width, style);
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        let word_width = word.chars().count();

        if word_width > width {
            if !line.is_empty() {
                lines.push(line);
                line = String::new();
            }
            split_long_word(word, width, &mut lines, &mut line);
            continue;
        }

        let next_width = if line.is_empty() {
            word_width
        } else {
            line.chars().count() + 1 + word_width
        };

        if next_width <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        } else {
            lines.push(line);
            line = word.to_string();
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }

    lines
}

fn split_long_word(word: &str, width: usize, lines: &mut Vec<String>, line: &mut String) {
    for char in word.chars() {
        if line.chars().count() == width {
            lines.push(std::mem::take(line));
        }
        line.push(char);
    }
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
        let visible = strip_ansi(&output.stdout);

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Doctor"));
        assert!(output.stdout.contains("\x1b[1;36m"));
        assert!(output.stdout.contains("\x1b[1;32m"));
        assert!(visible.contains("╭"));
        assert!(visible.contains("Status: ok"));
        assert!(visible.contains("[ok] dependency graph"));
        assert_lines_fit(&output.stdout, DEFAULT_RICH_WIDTH);
    }

    #[test]
    fn doctor_reports_missing_workspace_as_actionable_error() {
        let test_workspace = TestWorkspace::new("gomo-doctor-command-test");

        let output = run(test_workspace.path(), OutputOptions::default())
            .expect("doctor should render discovery errors");
        let visible = strip_ansi(&output.stdout);

        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.contains("\x1b[1;31m"));
        assert!(visible.contains("[error] workspace discovery"));
        assert!(visible.contains("add"));
        assert!(visible.contains("gomo.toml"));
        assert_lines_fit(&output.stdout, DEFAULT_RICH_WIDTH);
    }

    #[test]
    fn doctor_ci_output_stays_plain() {
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
                json: false,
                ci: true,
                tui: false,
                terminal_width: None,
            },
        )
        .expect("doctor should render plain CI output");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Doctor"));
        assert!(output.stdout.contains("Status: ok"));
        assert!(output.stdout.contains("[ok] dependency graph"));
        assert!(!output.stdout.contains("\x1b["));
        assert!(!output.stdout.contains("╭"));
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
                terminal_width: None,
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
