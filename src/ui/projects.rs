use ratatui::{
    layout::Constraint,
    widgets::{Block, BorderType, Cell as TableCell, Padding, Row, Table},
};

use crate::{
    gleam_toml::{DependencyTable, GleamPathDependency},
    workspace::Workspace,
};

use super::terminal;

const WIDE_TABLE_WIDTH: u16 = 120;
const COMPACT_TABLE_MIN_WIDTH: u16 = 70;
const COMPACT_NAME_WIDTH: usize = 18;
const COMPACT_TARGET_WIDTH: usize = 10;
const RESET: &str = "\x1b[0m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const DIM_GRAY: &str = "\x1b[2;90m";

pub(crate) fn render(workspace: &Workspace, terminal_width: Option<u16>) -> String {
    if workspace.projects.is_empty() {
        return format!(
            "No Gleam projects found under {}.\n",
            workspace.project_globs.join(", ")
        );
    }

    let width = terminal_width.unwrap_or(WIDE_TABLE_WIDTH);
    if width >= WIDE_TABLE_WIDTH {
        return render_wide_table(workspace);
    }

    if width >= COMPACT_TABLE_MIN_WIDTH {
        return render_compact_table(workspace, width as usize);
    }

    render_project_list(workspace, width as usize)
}

fn render_wide_table(workspace: &Workspace) -> String {
    let rows = workspace
        .projects
        .iter()
        .map(|project| {
            Row::new(vec![
                TableCell::from(project.name.clone()),
                terminal::separator_cell(),
                TableCell::from(project.target.clone()),
                terminal::separator_cell(),
                TableCell::from(project.root_relative_path.display().to_string()),
                terminal::separator_cell(),
                TableCell::from(render_path_dependencies(
                    project.path_dependencies.as_slice(),
                )),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(1),
            Constraint::Length(12),
            Constraint::Length(1),
            Constraint::Length(28),
            Constraint::Length(1),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(vec![
        terminal::header_cell("Name"),
        terminal::separator_cell(),
        terminal::header_cell("Target"),
        terminal::separator_cell(),
        terminal::header_cell("Root"),
        terminal::separator_cell(),
        terminal::header_cell("Dependencies"),
    ]))
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(terminal::line_style())
            .padding(Padding::horizontal(1)),
    )
    .column_spacing(1);

    terminal::render_widget_to_string(table, WIDE_TABLE_WIDTH, workspace.projects.len() as u16 + 3)
}

fn render_compact_table(workspace: &Workspace, width: usize) -> String {
    let content_width = content_width(width);
    let root_width = content_width
        .saturating_sub(COMPACT_NAME_WIDTH)
        .saturating_sub(COMPACT_TARGET_WIDTH)
        .saturating_sub(4)
        .max(1);
    let mut output = String::new();

    push_border_line(&mut output, '╭', '─', '╮', width);
    push_bordered_line(&mut output, "Projects", width, Some(BOLD_CYAN));
    push_separator_line(&mut output, width);
    push_bordered_line(
        &mut output,
        &format!(
            "{:<name_width$}  {:<target_width$}  {}",
            "Name",
            "Target",
            "Root",
            name_width = COMPACT_NAME_WIDTH,
            target_width = COMPACT_TARGET_WIDTH,
        ),
        width,
        Some(BOLD_CYAN),
    );

    for project in &workspace.projects {
        push_bordered_line(
            &mut output,
            &format!(
                "{:<name_width$}  {:<target_width$}  {}",
                truncate(&project.name, COMPACT_NAME_WIDTH),
                truncate(&project.target, COMPACT_TARGET_WIDTH),
                truncate(
                    &project.root_relative_path.display().to_string(),
                    root_width
                ),
                name_width = COMPACT_NAME_WIDTH,
                target_width = COMPACT_TARGET_WIDTH,
            ),
            width,
            None,
        );

        let dependencies = render_path_dependencies(project.path_dependencies.as_slice());
        if dependencies != "-" {
            push_bordered_line(
                &mut output,
                &format!(
                    "  deps: {}",
                    truncate(&dependencies, content_width.saturating_sub(8))
                ),
                width,
                None,
            );
        }
    }
    push_border_line(&mut output, '╰', '─', '╯', width);

    output
}

fn render_project_list(workspace: &Workspace, width: usize) -> String {
    let content_width = content_width(width);
    let mut output = String::new();

    push_border_line(&mut output, '╭', '─', '╮', width);
    push_bordered_line(&mut output, "Projects", width, Some(BOLD_CYAN));

    for (index, project) in workspace.projects.iter().enumerate() {
        push_separator_line(&mut output, width);
        push_bordered_line(&mut output, &project.name, width, Some(BOLD_YELLOW));
        push_project_field(&mut output, "target", &project.target, width);
        push_project_field(
            &mut output,
            "root",
            &project.root_relative_path.display().to_string(),
            width,
        );
        push_project_field(
            &mut output,
            "deps",
            &render_path_dependencies(project.path_dependencies.as_slice()),
            width,
        );

        if index + 1 < workspace.projects.len() && content_width > 0 {
            push_bordered_line(&mut output, "", width, None);
        }
    }

    push_border_line(&mut output, '╰', '─', '╯', width);

    output
}

fn push_project_field(output: &mut String, label: &str, value: &str, width: usize) {
    let prefix = format!("{label}: ");
    let prefix_width = prefix.chars().count();
    let content_width = content_width(width);

    if content_width < prefix_width {
        push_bordered_line(output, &format!("{label}: {value}"), width, None);
        return;
    }

    push_bordered_line(
        output,
        &format!(
            "{}{}",
            prefix,
            truncate(value, content_width - prefix_width)
        ),
        width,
        None,
    );
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

pub(crate) fn render_plain(workspace: &Workspace) -> String {
    if workspace.projects.is_empty() {
        return format!(
            "No Gleam projects found under {}.\n",
            workspace.project_globs.join(", ")
        );
    }

    let mut output = String::new();
    output.push_str("Projects\n");
    output.push_str("Name\tTarget\tRoot\tDependencies\n");
    for project in &workspace.projects {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            project.name,
            project.target,
            project.root_relative_path.display(),
            render_path_dependencies(project.path_dependencies.as_slice())
        ));
    }
    output
}

fn render_path_dependencies(dependencies: &[GleamPathDependency]) -> String {
    if dependencies.is_empty() {
        return "-".to_string();
    }

    dependencies
        .iter()
        .map(|dependency| match dependency.table {
            DependencyTable::Dependencies => dependency.name.clone(),
            DependencyTable::DevDependencies => format!("{}@dev", dependency.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::{
        gleam_toml::{DependencyTable, GleamPathDependency},
        workspace::{DefaultParallelism, DependencyVersionConfig, Project, Workspace},
    };

    use super::*;

    #[test]
    fn wide_projects_output_keeps_rich_table() {
        let output = render(&workspace_fixture(), Some(120));

        assert!(output.contains("\x1b[1;36m"));
        assert!(output.contains("Dependencies"));
        assert!(output.contains("│"));
        assert!(output.contains("shared"));
    }

    #[test]
    fn compact_projects_output_fits_medium_width() {
        let output = render(&workspace_fixture(), Some(80));
        let visible = strip_ansi(&output);

        assert!(output.contains("Projects"));
        assert!(output.contains("Name"));
        assert!(output.contains("Target"));
        assert!(output.contains("Root"));
        assert!(output.contains("  deps: shared"));
        assert!(output.contains("\x1b[1;36m"));
        assert!(visible.contains("╭"));
        assert!(visible.contains("│"));
        assert_lines_fit(&output, 80);
    }

    #[test]
    fn narrow_projects_output_uses_vertical_list() {
        let output = render(&workspace_fixture(), Some(50));
        let visible = strip_ansi(&output);

        assert!(output.contains("Projects"));
        assert!(output.contains("\x1b[1;36m"));
        assert!(output.contains("\x1b[1;33m"));
        assert!(visible.contains("╭"));
        assert!(visible.contains("│ demo"));
        assert!(visible.contains("target: javascript"));
        assert!(visible.contains("root: apps/demo"));
        assert!(visible.contains("deps: shared"));
        assert!(visible.contains("│ shared"));
        assert!(visible.contains("target: erlang"));
        assert!(visible.contains("root: libs/shared"));
        assert!(visible.contains("deps: -"));
        assert!(!visible.contains("Name"));
        assert_lines_fit(&output, 50);
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

    fn workspace_fixture() -> Workspace {
        Workspace {
            root: PathBuf::from("/workspace"),
            cache_dir: PathBuf::from("/workspace/.gomo/cache"),
            cache_max_age_seconds: None,
            cache_max_size_bytes: None,
            project_globs: vec!["apps/*".to_string(), "libs/*".to_string()],
            default_parallelism: DefaultParallelism::Auto,
            global_target_inputs: BTreeMap::new(),
            dependency_versions: DependencyVersionConfig {
                enabled: false,
                include_local: true,
                ignore: Vec::new(),
            },
            projects: vec![
                project(
                    "demo",
                    "javascript",
                    "apps/demo",
                    vec![GleamPathDependency {
                        name: "shared".to_string(),
                        path: PathBuf::from("../../libs/shared"),
                        table: DependencyTable::Dependencies,
                    }],
                ),
                project("shared", "erlang", "libs/shared", Vec::new()),
            ],
        }
    }

    fn project(
        name: &str,
        target: &str,
        root_relative_path: &str,
        path_dependencies: Vec<GleamPathDependency>,
    ) -> Project {
        Project {
            name: name.to_string(),
            version: Some("0.1.0".to_string()),
            target: target.to_string(),
            root: PathBuf::from("/workspace").join(root_relative_path),
            root_relative_path: PathBuf::from(root_relative_path),
            manifest_path: PathBuf::from("/workspace")
                .join(root_relative_path)
                .join("gleam.toml"),
            path_dependencies,
            gomo_targets: BTreeMap::new(),
        }
    }
}
