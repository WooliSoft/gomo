use ratatui::{
    layout::Constraint,
    widgets::{Block, BorderType, Cell as TableCell, Padding, Row, Table},
};

use crate::{
    gleam_toml::{DependencyTable, GleamPathDependency},
    workspace::Workspace,
};

use super::terminal;

const PROJECTS_TABLE_WIDTH: u16 = 120;

pub(crate) fn render(workspace: &Workspace) -> String {
    if workspace.projects.is_empty() {
        return format!(
            "No Gleam projects found under {}.\n",
            workspace.project_globs.join(", ")
        );
    }

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

    terminal::render_widget_to_string(
        table,
        PROJECTS_TABLE_WIDTH,
        workspace.projects.len() as u16 + 3,
    )
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
