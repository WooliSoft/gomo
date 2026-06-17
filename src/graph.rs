use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::gleam_toml::{DependencyTable, GleamPathDependency};
use crate::workspace::{Project, Workspace};

/// A validated dependency graph for discovered workspace projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGraph {
    /// Projects this project depends on.
    pub upstream: BTreeMap<String, Vec<String>>,
    /// Projects that depend on this project.
    pub downstream: BTreeMap<String, Vec<String>>,
    /// Dev-only projects this project depends on.
    pub dev_upstream: BTreeMap<String, Vec<String>>,
    /// Projects that depend on this project through dev-dependencies.
    pub dev_downstream: BTreeMap<String, Vec<String>>,
    /// Projects ordered so dependencies appear before dependents.
    pub topological_order: Vec<String>,
    /// Projects ordered so regular and dev dependencies appear before dependents.
    pub topological_order_with_dev: Vec<String>,
    /// Path dependencies that point at Gleam packages outside configured project roots.
    pub unmanaged_path_dependencies: BTreeMap<String, Vec<UnmanagedPathDependency>>,
}

/// A valid local path dependency that is not part of the discovered workspace graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmanagedPathDependency {
    /// Dependency package name as declared in `gleam.toml`.
    pub name: String,
    /// Dependency root path relative to the workspace root.
    pub root_relative_path: PathBuf,
}

enum ResolvedDependency {
    Managed(String),
    Unmanaged(UnmanagedPathDependency),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

impl ProjectGraph {
    /// Build and validate a project graph from a discovered workspace.
    pub fn build(workspace: &Workspace) -> Result<Self> {
        let project_roots = project_root_index(workspace)?;
        let mut upstream = BTreeMap::new();
        let mut downstream = BTreeMap::new();
        let mut dev_upstream = BTreeMap::new();
        let mut dev_downstream = BTreeMap::new();
        let mut unmanaged_path_dependencies =
            BTreeMap::<String, Vec<UnmanagedPathDependency>>::new();

        for project in &workspace.projects {
            upstream.insert(project.name.clone(), Vec::new());
            downstream.insert(project.name.clone(), Vec::new());
            dev_upstream.insert(project.name.clone(), Vec::new());
            dev_downstream.insert(project.name.clone(), Vec::new());
        }

        for project in &workspace.projects {
            for dependency in &project.path_dependencies {
                match resolve_dependency(workspace, project, dependency, &project_roots)? {
                    ResolvedDependency::Managed(dependency_project) => {
                        let (upstream, downstream) = match dependency.table {
                            DependencyTable::Dependencies => (&mut upstream, &mut downstream),
                            DependencyTable::DevDependencies => {
                                (&mut dev_upstream, &mut dev_downstream)
                            }
                        };
                        insert_dependency_edge(
                            upstream,
                            downstream,
                            project.name.as_str(),
                            dependency_project.as_str(),
                        );
                    }
                    ResolvedDependency::Unmanaged(dependency) => {
                        unmanaged_path_dependencies
                            .entry(project.name.clone())
                            .or_default()
                            .push(dependency);
                    }
                }
            }
        }

        for dependencies in upstream.values_mut() {
            dependencies.sort();
        }
        for dependents in downstream.values_mut() {
            dependents.sort();
        }
        for dependencies in dev_upstream.values_mut() {
            dependencies.sort();
        }
        for dependents in dev_downstream.values_mut() {
            dependents.sort();
        }
        for dependencies in unmanaged_path_dependencies.values_mut() {
            dependencies.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.root_relative_path.cmp(&right.root_relative_path))
            });
        }

        let upstream_with_dev = merge_dependency_maps(&upstream, &dev_upstream);
        let downstream_with_dev = merge_dependency_maps(&downstream, &dev_downstream);

        if let Some(cycle) = detect_cycle(&upstream_with_dev) {
            bail!("dependency cycle detected: {}", cycle.join(" -> "));
        }

        let dependency_order = topological_order(&upstream, &downstream)?;
        let topological_order_with_dev =
            topological_order(&upstream_with_dev, &downstream_with_dev)?;

        Ok(Self {
            upstream,
            downstream,
            dev_upstream,
            dev_downstream,
            topological_order: dependency_order,
            topological_order_with_dev,
            unmanaged_path_dependencies,
        })
    }

    /// Return projects this project depends on.
    pub fn upstream_for(&self, project: &str) -> &[String] {
        self.upstream.get(project).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Return projects that depend on this project.
    pub fn downstream_for(&self, project: &str) -> &[String] {
        self.downstream
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Return dev-only projects this project depends on.
    pub fn dev_upstream_for(&self, project: &str) -> &[String] {
        self.dev_upstream
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Return projects that depend on this project through dev-dependencies.
    pub fn dev_downstream_for(&self, project: &str) -> &[String] {
        self.dev_downstream
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Return regular plus dev-only projects this project depends on.
    pub fn upstream_with_dev_for(&self, project: &str) -> Vec<String> {
        merge_project_lists(self.upstream_for(project), self.dev_upstream_for(project))
    }

    /// Return regular plus dev-only projects that depend on this project.
    pub fn downstream_with_dev_for(&self, project: &str) -> Vec<String> {
        merge_project_lists(
            self.downstream_for(project),
            self.dev_downstream_for(project),
        )
    }

    /// Return unmanaged local path dependencies declared by this project.
    pub fn unmanaged_for(&self, project: &str) -> &[UnmanagedPathDependency] {
        self.unmanaged_path_dependencies
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn project_root_index(workspace: &Workspace) -> Result<BTreeMap<PathBuf, String>> {
    let mut project_roots = BTreeMap::new();

    for project in &workspace.projects {
        let root = project.root.canonicalize().with_context(|| {
            format!(
                "failed to resolve project root {} for `{}`",
                project.root.display(),
                project.name
            )
        })?;
        project_roots.insert(root, project.name.clone());
    }

    Ok(project_roots)
}

fn resolve_dependency(
    workspace: &Workspace,
    project: &Project,
    dependency: &GleamPathDependency,
    project_roots: &BTreeMap<PathBuf, String>,
) -> Result<ResolvedDependency> {
    let dependency_root = project
        .root
        .join(&dependency.path)
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to resolve path dependency `{}` in {}: {}",
                dependency.name,
                project.manifest_path.display(),
                dependency.path.display()
            )
        })?;

    if !dependency_root.starts_with(&workspace.root) {
        bail!(
            "path dependency `{}` in {} resolves outside the workspace: {}",
            dependency.name,
            project.manifest_path.display(),
            dependency_root.display()
        );
    }

    if !dependency_root.join("gleam.toml").is_file() {
        bail!(
            "path dependency `{}` in {} resolves to {}, but no gleam.toml was found there",
            dependency.name,
            project.manifest_path.display(),
            dependency_root.display()
        );
    }

    if let Some(project_name) = project_roots.get(&dependency_root) {
        if project_name != &dependency.name {
            bail!(
                "path dependency `{}` in {} points to workspace project `{}` at {}",
                dependency.name,
                project.manifest_path.display(),
                project_name,
                dependency_root.display()
            );
        }

        return Ok(ResolvedDependency::Managed(project_name.clone()));
    }

    let root_relative_path = dependency_root
        .strip_prefix(&workspace.root)
        .unwrap_or(dependency_root.as_path())
        .to_path_buf();

    Ok(ResolvedDependency::Unmanaged(UnmanagedPathDependency {
        name: dependency.name.clone(),
        root_relative_path,
    }))
}

fn insert_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn insert_dependency_edge(
    upstream: &mut BTreeMap<String, Vec<String>>,
    downstream: &mut BTreeMap<String, Vec<String>>,
    project: &str,
    dependency: &str,
) {
    insert_unique(
        upstream
            .get_mut(project)
            .expect("project was initialized in upstream map"),
        dependency.to_string(),
    );
    insert_unique(
        downstream
            .get_mut(dependency)
            .expect("dependency project was initialized in downstream map"),
        project.to_string(),
    );
}

fn merge_dependency_maps(
    left: &BTreeMap<String, Vec<String>>,
    right: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut merged = left.clone();

    for (project, dependencies) in right {
        let merged_dependencies = merged.entry(project.clone()).or_default();
        for dependency in dependencies {
            insert_unique(merged_dependencies, dependency.clone());
        }
        merged_dependencies.sort();
    }

    merged
}

fn merge_project_lists(left: &[String], right: &[String]) -> Vec<String> {
    let mut merged = left.to_vec();
    for project in right {
        insert_unique(&mut merged, project.clone());
    }
    merged.sort();
    merged
}

fn detect_cycle(upstream: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();

    for project in upstream.keys() {
        if states.get(project) == Some(&VisitState::Visited) {
            continue;
        }

        if let Some(cycle) = visit_for_cycle(project, upstream, &mut states, &mut stack) {
            return Some(cycle);
        }
    }

    None
}

fn visit_for_cycle(
    project: &str,
    upstream: &BTreeMap<String, Vec<String>>,
    states: &mut BTreeMap<String, VisitState>,
    stack: &mut Vec<String>,
) -> Option<Vec<String>> {
    match states.get(project) {
        Some(VisitState::Visiting) => {
            let start = stack.iter().position(|name| name == project).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(project.to_string());
            return Some(cycle);
        }
        Some(VisitState::Visited) => return None,
        None => {}
    }

    states.insert(project.to_string(), VisitState::Visiting);
    stack.push(project.to_string());

    if let Some(dependencies) = upstream.get(project) {
        for dependency in dependencies {
            if let Some(cycle) = visit_for_cycle(dependency, upstream, states, stack) {
                return Some(cycle);
            }
        }
    }

    stack.pop();
    states.insert(project.to_string(), VisitState::Visited);
    None
}

fn topological_order(
    upstream: &BTreeMap<String, Vec<String>>,
    downstream: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<String>> {
    let mut remaining_dependencies = upstream
        .iter()
        .map(|(project, dependencies)| (project.clone(), dependencies.len()))
        .collect::<BTreeMap<_, _>>();
    let mut ready = remaining_dependencies
        .iter()
        .filter_map(|(project, count)| (*count == 0).then_some(project.clone()))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::new();

    while let Some(project) = pop_first(&mut ready) {
        order.push(project.clone());

        if let Some(dependents) = downstream.get(&project) {
            for dependent in dependents {
                let count = remaining_dependencies
                    .get_mut(dependent)
                    .expect("dependent project exists in dependency count map");
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }

    if order.len() != upstream.len() {
        bail!("dependency graph could not be topologically sorted");
    }

    Ok(order)
}

fn pop_first(values: &mut BTreeSet<String>) -> Option<String> {
    let value = values.iter().next()?.clone();
    values.remove(&value);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    #[test]
    fn builds_upstream_downstream_maps_and_topological_order() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
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

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");

        assert_eq!(
            graph.topological_order,
            vec!["protocol", "renderer", "demo"]
        );
        assert_eq!(graph.upstream_for("demo"), ["protocol", "renderer"]);
        assert_eq!(graph.upstream_for("renderer"), ["protocol"]);
        assert_eq!(graph.upstream_for("protocol"), [] as [&str; 0]);
        assert_eq!(graph.downstream_for("protocol"), ["demo", "renderer"]);
        assert_eq!(graph.downstream_for("renderer"), ["demo"]);
    }

    #[test]
    fn links_path_dependencies_outside_project_roots_after_discovery() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
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

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");

        assert_eq!(graph.topological_order, vec!["esgleam", "demo"]);
        assert_eq!(graph.upstream_for("demo"), ["esgleam"]);
        assert!(graph.unmanaged_for("demo").is_empty());
    }

    #[test]
    fn configured_project_roots_make_tools_dependencies_managed() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*", "tools/*"]
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

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");

        assert_eq!(graph.topological_order, vec!["esgleam", "demo"]);
        assert_eq!(graph.upstream_for("demo"), ["esgleam"]);
        assert!(graph.unmanaged_for("demo").is_empty());
    }

    #[test]
    fn records_dev_path_dependencies_separately_from_build_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
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

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");

        assert_eq!(graph.upstream_for("demo"), [] as [&str; 0]);
        assert_eq!(graph.dev_upstream_for("demo"), ["test_support"]);
        assert_eq!(graph.downstream_for("test_support"), [] as [&str; 0]);
        assert_eq!(graph.dev_downstream_for("test_support"), ["demo"]);
        assert_eq!(graph.topological_order_with_dev, ["test_support", "demo"]);
    }

    #[test]
    fn rejects_path_dependencies_outside_workspace() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        let outside_workspace = TestWorkspace::new("gomo-outside-graph-test");
        test_workspace.write_gomo_config();
        outside_workspace.write_manifest(
            ".",
            r#"
name = "outside"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "apps/demo",
            &format!(
                r#"
name = "demo"
version = "0.1.0"

[dependencies]
outside = {{ path = "{}" }}
"#,
                outside_workspace.path().display()
            ),
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = ProjectGraph::build(&workspace).expect_err("outside dependency should fail");

        assert!(error.to_string().contains("resolves outside the workspace"));
        assert!(error.to_string().contains("outside"));
    }

    #[test]
    fn rejects_path_dependencies_without_manifest() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        test_workspace.write_gomo_config();
        test_workspace.write_file("libs/not_a_package/README.md", "not a package");
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
not_a_package = { path = "../../libs/not_a_package" }
"#,
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = ProjectGraph::build(&workspace).expect_err("missing manifest should fail");

        assert!(error.to_string().contains("no gleam.toml was found"));
        assert!(error.to_string().contains("not_a_package"));
    }

    #[test]
    fn rejects_cycles_with_clear_path() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/one",
            r#"
name = "one"
version = "0.1.0"

[dependencies]
two = { path = "../../libs/two" }
"#,
        );
        test_workspace.write_manifest(
            "libs/two",
            r#"
name = "two"
version = "0.1.0"

[dependencies]
one = { path = "../../apps/one" }
"#,
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = ProjectGraph::build(&workspace).expect_err("cycle should fail");

        assert_eq!(
            error.to_string(),
            "dependency cycle detected: one -> two -> one"
        );
    }

    #[test]
    fn rejects_dependency_names_that_do_not_match_workspace_project_names() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
wrong_name = { path = "../../libs/shared" }
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = ProjectGraph::build(&workspace).expect_err("mismatched name should fail");

        assert!(
            error
                .to_string()
                .contains("points to workspace project `shared`")
        );
        assert!(error.to_string().contains("wrong_name"));
    }

    #[test]
    fn rejects_missing_dependency_paths_with_context() {
        let test_workspace = TestWorkspace::new("gomo-graph-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[dependencies]
missing = { path = "../../libs/missing" }
"#,
        );

        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let error = ProjectGraph::build(&workspace).expect_err("missing path should fail");
        let message = format!("{error:#}");

        assert!(message.contains("failed to resolve path dependency `missing`"));
        assert!(message.contains("../../libs/missing"));
    }
}
