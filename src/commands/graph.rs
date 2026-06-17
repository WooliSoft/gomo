use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    commands::{CommandOutput, OutputOptions},
    graph::{ProjectGraph, UnmanagedPathDependency},
    ui, workspace,
};

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let output = if output_options.json {
        render_json(&workspace, &graph)?
    } else if output_options.ci {
        ui::graph::render_plain(&workspace, &graph)
    } else {
        ui::graph::render(&workspace, &graph)
    };

    Ok(CommandOutput::success(output))
}

fn render_json(workspace: &workspace::Workspace, graph: &ProjectGraph) -> Result<String> {
    let output = GraphJson::from((workspace, graph));
    let mut json =
        serde_json::to_string_pretty(&output).context("failed to serialize graph JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct GraphJson {
    workspace_root: String,
    topological_order: Vec<String>,
    projects: Vec<GraphProjectJson>,
}

#[derive(Serialize)]
struct GraphProjectJson {
    name: String,
    upstream: Vec<String>,
    downstream: Vec<String>,
    unmanaged_path_dependencies: Vec<UnmanagedPathDependencyJson>,
}

#[derive(Serialize)]
struct UnmanagedPathDependencyJson {
    name: String,
    root: String,
}

impl From<(&workspace::Workspace, &ProjectGraph)> for GraphJson {
    fn from((workspace, graph): (&workspace::Workspace, &ProjectGraph)) -> Self {
        Self {
            workspace_root: workspace.root.display().to_string(),
            topological_order: graph.topological_order.clone(),
            projects: graph
                .topological_order
                .iter()
                .map(|project| GraphProjectJson {
                    name: project.clone(),
                    upstream: graph.upstream_for(project).to_vec(),
                    downstream: graph.downstream_for(project).to_vec(),
                    unmanaged_path_dependencies: graph
                        .unmanaged_for(project)
                        .iter()
                        .map(UnmanagedPathDependencyJson::from)
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<&UnmanagedPathDependency> for UnmanagedPathDependencyJson {
    fn from(dependency: &UnmanagedPathDependency) -> Self {
        Self {
            name: dependency.name.clone(),
            root: dependency.root_relative_path.display().to_string(),
        }
    }
}
