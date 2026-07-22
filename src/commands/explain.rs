use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache::{self, TaskHash};
use crate::commands::{CommandOutput, OutputOptions};
use crate::graph::ProjectGraph;
use crate::runner::Target;
use crate::workspace::{self, Project, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplainRequest {
    pub(crate) target: Target,
    pub(crate) project: String,
}

pub(crate) fn run(
    cwd: &Path,
    request: ExplainRequest,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let project = find_project(&workspace, &request.project)?;
    let task_hash = cache::compute_task_hash(&workspace, &graph, project, request.target)?;

    if output_options.json {
        return Ok(CommandOutput::success(render_explain_json(&task_hash)?));
    }

    Ok(CommandOutput::success(render_explain(&task_hash)))
}

fn find_project<'a>(workspace: &'a Workspace, project_name: &str) -> Result<&'a Project> {
    if let Some(project) = workspace
        .projects
        .iter()
        .find(|project| project.name == project_name)
    {
        return Ok(project);
    }

    let known_projects = workspace
        .projects
        .iter()
        .map(|project| project.name.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    bail!("unknown project `{project_name}`. Known projects: {known_projects}")
}

fn render_explain(task_hash: &TaskHash) -> String {
    let mut output = String::new();
    output.push_str("Cache Key\n");
    output.push_str(&format!("Project: {}\n", task_hash.project));
    output.push_str(&format!("Project Root: {}\n", task_hash.project_root));
    output.push_str(&format!("Project Target: {}\n", task_hash.project_target));
    output.push_str(&format!("Target: {}\n", task_hash.target));
    output.push_str(&format!("Command: {}\n", task_hash.command));
    output.push_str(&format!("Hash: {}\n", task_hash.hash));
    output.push_str(&format!("Schema: {}\n", task_hash.schema_version));
    output.push_str(&format!("Gomo Version: {}\n", task_hash.gomo_version));
    output.push_str(&format!("Gleam Version: {}\n", task_hash.gleam_version));
    output.push_str(&format!(
        "Input Source: {}\n",
        task_hash.input_source.as_str()
    ));
    output.push_str(&format!("Manifest Hash: {}\n", task_hash.manifest_hash));

    output.push_str("Input Globs:\n");
    for input_glob in &task_hash.input_globs {
        output.push_str(&format!("- {input_glob}\n"));
    }

    output.push_str("Workspace Input Globs:\n");
    if task_hash.workspace_input_globs.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for input_glob in &task_hash.workspace_input_globs {
            output.push_str(&format!("- {input_glob}\n"));
        }
    }

    output.push_str("Cached Folders:\n");
    if task_hash.cached_folders.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for folder in &task_hash.cached_folders {
            output.push_str(&format!("- {folder}\n"));
        }
    }

    output.push_str("Matched Inputs:\n");
    if task_hash.input_files.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for input_file in &task_hash.input_files {
            output.push_str(&format!(
                "- {} {} {} bytes\n",
                input_file.relative_path, input_file.content_hash, input_file.byte_len
            ));
        }
    }

    output.push_str("Matched Workspace Inputs:\n");
    if task_hash.workspace_input_files.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for input_file in &task_hash.workspace_input_files {
            output.push_str(&format!(
                "- {} {} {} bytes\n",
                input_file.relative_path, input_file.content_hash, input_file.byte_len
            ));
        }
    }

    output.push_str("Dependency Task Hashes:\n");
    if task_hash.dependency_hashes.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for dependency_hash in &task_hash.dependency_hashes {
            output.push_str(&format!(
                "- {}:{} {}\n",
                dependency_hash.project, dependency_hash.target, dependency_hash.hash
            ));
        }
    }

    output.push_str("Environment:\n");
    if task_hash.environment.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for environment_input in &task_hash.environment {
            output.push_str(&format!(
                "- {}={}\n",
                environment_input.name, environment_input.value
            ));
        }
    }

    output
}

fn render_explain_json(task_hash: &TaskHash) -> Result<String> {
    let output = ExplainJson::from(task_hash);
    let mut json =
        serde_json::to_string_pretty(&output).context("failed to serialize explain JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct ExplainJson<'a> {
    project: &'a str,
    project_root: &'a str,
    project_target: &'a str,
    target: &'static str,
    command: &'a str,
    hash: &'a str,
    schema_version: &'a str,
    gomo_version: &'a str,
    gleam_version: &'a str,
    input_source: &'static str,
    manifest_hash: &'a str,
    input_globs: &'a [String],
    workspace_input_globs: &'a [String],
    cached_folders: &'a [String],
    input_files: Vec<InputFileJson<'a>>,
    workspace_input_files: Vec<InputFileJson<'a>>,
    dependency_hashes: Vec<DependencyHashJson<'a>>,
    environment: Vec<EnvironmentJson<'a>>,
}

#[derive(Serialize)]
struct InputFileJson<'a> {
    relative_path: &'a str,
    content_hash: &'a str,
    byte_len: u64,
}

#[derive(Serialize)]
struct DependencyHashJson<'a> {
    project: &'a str,
    target: &'static str,
    hash: &'a str,
}

#[derive(Serialize)]
struct EnvironmentJson<'a> {
    name: &'a str,
    value: &'a str,
}

impl<'a> From<&'a TaskHash> for ExplainJson<'a> {
    fn from(task_hash: &'a TaskHash) -> Self {
        Self {
            project: &task_hash.project,
            project_root: &task_hash.project_root,
            project_target: &task_hash.project_target,
            target: task_hash.target.as_str(),
            command: &task_hash.command,
            hash: &task_hash.hash,
            schema_version: &task_hash.schema_version,
            gomo_version: &task_hash.gomo_version,
            gleam_version: &task_hash.gleam_version,
            input_source: task_hash.input_source.as_str(),
            manifest_hash: &task_hash.manifest_hash,
            input_globs: &task_hash.input_globs,
            workspace_input_globs: &task_hash.workspace_input_globs,
            cached_folders: &task_hash.cached_folders,
            input_files: task_hash
                .input_files
                .iter()
                .map(|input| InputFileJson {
                    relative_path: &input.relative_path,
                    content_hash: &input.content_hash,
                    byte_len: input.byte_len,
                })
                .collect(),
            workspace_input_files: task_hash
                .workspace_input_files
                .iter()
                .map(|input| InputFileJson {
                    relative_path: &input.relative_path,
                    content_hash: &input.content_hash,
                    byte_len: input.byte_len,
                })
                .collect(),
            dependency_hashes: task_hash
                .dependency_hashes
                .iter()
                .map(|dependency| DependencyHashJson {
                    project: &dependency.project,
                    target: dependency.target.as_str(),
                    hash: &dependency.hash,
                })
                .collect(),
            environment: task_hash
                .environment
                .iter()
                .map(|environment| EnvironmentJson {
                    name: &environment.name,
                    value: &environment.value,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::compute_task_hash_with_gleam_version;
    use crate::test_support::TestWorkspace;

    #[test]
    fn renders_cache_explain_output() {
        let test_workspace = TestWorkspace::new("gomo-explain-command-test");
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
        test_workspace.write_file("apps/demo/src/main.gleam", "pub fn main() { Nil }\n");
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project = find_project(&workspace, "demo").expect("project should exist");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            project,
            Target::Test,
            "gleam 1.0.0",
        )
        .expect("hash should compute");
        let output = render_explain(&task_hash);

        assert!(output.contains("Cache Key"));
        assert!(output.contains("Project: demo"));
        assert!(output.contains("Target: test"));
        assert!(output.contains("Command: gleam test"));
        assert!(output.contains("Input Globs:"));
        assert!(output.contains("- test/**"));
        assert!(output.contains("Cached Folders:\n- (none)"));
        assert!(output.contains("Matched Inputs:"));
        assert!(output.contains("src/main.gleam"));
        assert!(output.contains("Dependency Task Hashes:"));
        assert!(output.contains("shared:build"));
    }

    #[test]
    fn renders_cache_explain_json() {
        let test_workspace = TestWorkspace::new("gomo-explain-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn value() { 1 }\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project = find_project(&workspace, "shared").expect("project should exist");
        let task_hash = compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            project,
            Target::Build,
            "gleam 1.0.0",
        )
        .expect("hash should compute");

        let json = render_explain_json(&task_hash).expect("JSON should render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSON should parse");

        assert_eq!(value["project"], "shared");
        assert_eq!(value["target"], "build");
        assert_eq!(value["input_source"], "built-in defaults");
        assert_eq!(value["cached_folders"][0], "build");
    }

    #[test]
    fn rejects_unknown_projects() {
        let test_workspace = TestWorkspace::new("gomo-explain-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = find_project(&workspace, "missing").expect_err("unknown project should fail");

        assert!(error.to_string().contains("unknown project `missing`"));
        assert!(error.to_string().contains("Known projects: demo"));
    }
}
