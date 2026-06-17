use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

use crate::cache;
use crate::graph::ProjectGraph;
use crate::runner::Target;
use crate::workspace::{Project, Workspace};

/// Select projects whose target inputs changed, plus their downstream dependents.
pub(crate) fn select_affected_projects(
    workspace: &Workspace,
    graph: &ProjectGraph,
    target: Target,
    changed_files: &[PathBuf],
) -> Result<Vec<String>> {
    if !target.supports_cache() {
        bail!("target `{target}` does not support affected input selection");
    }

    for changed_file in changed_files {
        validate_workspace_relative_path(changed_file)?;
        if cache::workspace_inputs_match(workspace, target, changed_file)? {
            return Ok(target_topological_order(graph, target).to_vec());
        }
    }

    let mut selected = BTreeSet::new();
    let project_index = project_index(workspace);

    for changed_file in changed_files {
        validate_workspace_relative_path(changed_file)?;

        let Some((project, project_relative_path)) = owning_project(workspace, changed_file) else {
            continue;
        };

        if cache::target_inputs_match(project, target, &project_relative_path)? {
            selected.insert(project.name.clone());
        }
    }

    if matches!(target, Target::Build | Target::Test) {
        for project in selected.clone() {
            include_downstream_dependents(&project, graph, &project_index, target, &mut selected);
        }
    }

    Ok(target_topological_order(graph, target)
        .iter()
        .filter(|project| selected.contains(*project))
        .cloned()
        .collect())
}

fn validate_workspace_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("changed file path must not be empty");
    }
    if path.is_absolute() {
        bail!(
            "changed file `{}` must be relative to the workspace root",
            path.display()
        );
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!(
                "changed file `{}` must not leave the workspace root",
                path.display()
            );
        }
    }

    Ok(())
}

fn owning_project<'a>(
    workspace: &'a Workspace,
    changed_file: &Path,
) -> Option<(&'a Project, PathBuf)> {
    workspace
        .projects
        .iter()
        .filter_map(|project| {
            changed_file
                .strip_prefix(&project.root_relative_path)
                .ok()
                .map(|relative_path| (project, relative_path.to_path_buf()))
        })
        .max_by_key(|(project, _)| project.root_relative_path.components().count())
}

fn include_downstream_dependents(
    project: &str,
    graph: &ProjectGraph,
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
    selected: &mut BTreeSet<String>,
) {
    for dependent in target_downstream_projects(graph, project_index, project, target) {
        if selected.insert(dependent.clone()) {
            include_downstream_dependents(&dependent, graph, project_index, target, selected);
        }
    }
}

fn target_downstream_projects(
    graph: &ProjectGraph,
    project_index: &BTreeMap<&str, &Project>,
    project: &str,
    target: Target,
) -> Vec<String> {
    match target {
        Target::Test => graph.downstream_with_dev_for(project),
        Target::Build => {
            let mut downstream = graph.downstream_for(project).to_vec();
            for dependent in graph.dev_downstream_for(project) {
                if uses_custom_build_command(project_index, dependent) {
                    insert_unique(&mut downstream, dependent.clone());
                }
            }
            downstream.sort();
            downstream
        }
        Target::Format | Target::Clean => Vec::new(),
    }
}

fn project_index(workspace: &Workspace) -> BTreeMap<&str, &Project> {
    workspace
        .projects
        .iter()
        .map(|project| (project.name.as_str(), project))
        .collect()
}

fn uses_custom_build_command(project_index: &BTreeMap<&str, &Project>, project: &str) -> bool {
    project_index
        .get(project)
        .and_then(|project| project.gomo_targets.get(Target::Build.as_str()))
        .and_then(|config| config.command.as_ref())
        .is_some()
}

fn insert_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn target_topological_order(graph: &ProjectGraph, target: Target) -> &[String] {
    match target {
        Target::Build | Target::Test => &graph.topological_order_with_dev,
        Target::Format | Target::Clean => &graph.topological_order,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    fn write_graph_fixture(test_workspace: &TestWorkspace) {
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
renderer = { path = "../../libs/renderer" }
protocol = { path = "../../libs/protocol" }
"#,
        );
        test_workspace.write_manifest(
            "libs/renderer",
            r#"
name = "renderer"
version = "0.1.0"

[dependencies]
protocol = { path = "../protocol" }
"#,
        );
        test_workspace.write_manifest(
            "libs/protocol",
            r#"
name = "protocol"
version = "0.1.0"
"#,
        );
    }

    fn load_workspace(test_workspace: &TestWorkspace) -> (Workspace, ProjectGraph) {
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        (workspace, graph)
    }

    #[test]
    fn source_change_selects_owner_and_downstream_dependents() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        write_graph_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Test,
            &[PathBuf::from("libs/protocol/src/main.gleam")],
        )
        .expect("affected projects should be selected");

        assert_eq!(selected, ["protocol", "renderer", "demo"]);
    }

    #[test]
    fn format_source_change_selects_only_owner() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        write_graph_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Format,
            &[PathBuf::from("libs/protocol/src/main.gleam")],
        )
        .expect("affected projects should be selected");

        assert_eq!(selected, ["protocol"]);
    }

    #[test]
    fn test_input_change_does_not_affect_build_target() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        write_graph_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Build,
            &[PathBuf::from("libs/protocol/test/protocol_test.gleam")],
        )
        .expect("affected projects should be selected");

        assert!(selected.is_empty());
    }

    #[test]
    fn project_input_overrides_drive_file_mapping() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"

[tools.gomo.test]
inputs = ["gleam.toml", "src/**", "fixtures/**"]
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Test,
            &[PathBuf::from("libs/shared/fixtures/example.txt")],
        )
        .expect("override input should be selected");

        assert_eq!(selected, ["shared"]);
    }

    #[test]
    fn workspace_input_change_selects_all_projects() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*", "libs/*"]

[workspace.test]
inputs = ["gomo.toml", ".github/workflows/**"]
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
version = "0.1.0"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Test,
            &[PathBuf::from(".github/workflows/test.yml")],
        )
        .expect("workspace input should select all projects");

        assert_eq!(selected, ["shared", "demo"]);
    }

    #[test]
    fn path_dependency_outside_project_roots_selects_dependency_and_dependents() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
esgleam = { path = "../../tools/esgleam" }
"#,
        );
        test_workspace.write_manifest(
            "tools/esgleam",
            r#"
name = "esgleam"
version = "0.1.0"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Build,
            &[PathBuf::from("tools/esgleam/src/main.gleam")],
        )
        .expect("referenced path dependency should be selected");

        assert_eq!(selected, ["esgleam", "demo"]);
    }

    #[test]
    fn default_build_ignores_dev_dependency_downstream_projects() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dev-dependencies]
test_support = { path = "../../tools/test_support" }
"#,
        );
        test_workspace.write_manifest(
            "tools/test_support",
            r#"
name = "test_support"
version = "0.1.0"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Build,
            &[PathBuf::from("tools/test_support/src/main.gleam")],
        )
        .expect("dev dependency project itself should be selected");

        assert_eq!(selected, ["test_support"]);
    }

    #[test]
    fn custom_build_includes_dev_dependency_downstream_projects() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dev-dependencies]
test_support = { path = "../../tools/test_support" }

[tools.gomo.build]
command = "make build"
"#,
        );
        test_workspace.write_manifest(
            "tools/test_support",
            r#"
name = "test_support"
version = "0.1.0"
"#,
        );
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Build,
            &[PathBuf::from("tools/test_support/src/main.gleam")],
        )
        .expect("custom build should include dev dependents");

        assert_eq!(selected, ["test_support", "demo"]);
    }

    #[test]
    fn files_outside_projects_are_ignored() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        write_graph_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);

        let selected = select_affected_projects(
            &workspace,
            &graph,
            Target::Test,
            &[PathBuf::from("README.md")],
        )
        .expect("outside project file should be ignored");

        assert!(selected.is_empty());
    }

    #[test]
    fn rejects_non_relative_changed_files() {
        let test_workspace = TestWorkspace::new("gomo-affected-test");
        write_graph_fixture(&test_workspace);
        let (workspace, graph) = load_workspace(&test_workspace);

        let error = select_affected_projects(
            &workspace,
            &graph,
            Target::Test,
            &[PathBuf::from("../outside.gleam")],
        )
        .expect_err("parent paths should be rejected");

        assert!(
            error
                .to_string()
                .contains("must not leave the workspace root")
        );
    }
}
