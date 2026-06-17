use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    commands::{CommandOutput, OutputOptions},
    gleam_toml::GleamPathDependency,
    ui,
    workspace::{self, Workspace},
};

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let output = if output_options.json {
        render_json(&workspace)?
    } else if output_options.ci {
        ui::projects::render_plain(&workspace)
    } else {
        ui::projects::render(&workspace)
    };

    Ok(CommandOutput::success(output))
}

fn render_json(workspace: &Workspace) -> Result<String> {
    let output = ProjectsJson::from(workspace);
    let mut json =
        serde_json::to_string_pretty(&output).context("failed to serialize projects JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct ProjectsJson {
    workspace_root: String,
    cache_dir: String,
    projects: Vec<ProjectJson>,
}

#[derive(Serialize)]
struct ProjectJson {
    name: String,
    target: String,
    root: String,
    path_dependencies: Vec<PathDependencyJson>,
}

#[derive(Serialize)]
struct PathDependencyJson {
    name: String,
    path: String,
    table: String,
}

impl From<&Workspace> for ProjectsJson {
    fn from(workspace: &Workspace) -> Self {
        Self {
            workspace_root: workspace.root.display().to_string(),
            cache_dir: workspace.cache_dir.display().to_string(),
            projects: workspace
                .projects
                .iter()
                .map(|project| ProjectJson {
                    name: project.name.clone(),
                    target: project.target.clone(),
                    root: project.root_relative_path.display().to_string(),
                    path_dependencies: project
                        .path_dependencies
                        .iter()
                        .map(PathDependencyJson::from)
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<&GleamPathDependency> for PathDependencyJson {
    fn from(dependency: &GleamPathDependency) -> Self {
        Self {
            name: dependency.name.clone(),
            path: dependency.path.display().to_string(),
            table: dependency.table.to_string(),
        }
    }
}
