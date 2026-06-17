use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::affected;
use crate::commands::{CommandOutput, OutputOptions};
use crate::graph::ProjectGraph;
use crate::runner::{CommandOptions, CommandRunner, GleamCommandRunner, Target};
use crate::vcs::{ChangedFileSource, ExplicitChangedFiles, VcsChangedFiles};
use crate::workspace;

use super::run::{self, CacheOptions, Parallelism};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AffectedRequest {
    pub(crate) target: Target,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) base: Option<String>,
    pub(crate) parallelism: Parallelism,
}

pub(crate) fn run(
    cwd: &Path,
    request: AffectedRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    run_with_runner_and_cache(
        cwd,
        request,
        &GleamCommandRunner,
        cache_options,
        output_options,
    )
}

fn run_with_runner_and_cache(
    cwd: &Path,
    request: AffectedRequest,
    runner: &(impl CommandRunner + Sync),
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let changed_files = changed_files_for_request(&workspace, &request)?;
    let project_names =
        affected::select_affected_projects(&workspace, &graph, request.target, &changed_files)?;

    run::run_project_names(
        &workspace,
        &graph,
        &project_names,
        request.target,
        CommandOptions::default(),
        runner,
        cache_options,
        request.parallelism,
        output_options,
    )
}

fn changed_files_for_request(
    workspace: &workspace::Workspace,
    request: &AffectedRequest,
) -> Result<Vec<PathBuf>> {
    match (request.files.is_empty(), request.base.as_deref()) {
        (false, None) => ExplicitChangedFiles::new(request.files.clone())?.changed_files(workspace),
        (true, Some(base)) => VcsChangedFiles::new(base.to_string()).changed_files(workspace),
        (false, Some(_)) => anyhow::bail!("affected accepts only one of --files or --base"),
        (true, None) => anyhow::bail!("affected requires either --files or --base"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::runner::TaskExecution;
    use crate::test_support::TestWorkspace;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            project: &workspace::Project,
            target: Target,
            _options: CommandOptions,
        ) -> TaskExecution {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .push(format!("{}:{target}", project.name));

            TaskExecution::success(format!("{} passed\n", project.name), "")
        }
    }

    fn write_graph_fixture(test_workspace: &TestWorkspace) {
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
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
    }

    #[test]
    fn affected_run_executes_changed_project_and_dependents() {
        let test_workspace = TestWorkspace::new("gomo-affected-command-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        let output = run_with_runner_and_cache(
            test_workspace.path(),
            AffectedRequest {
                target: Target::Build,
                files: vec![PathBuf::from("libs/shared/src/main.gleam")],
                base: None,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect("affected tasks should run");

        assert_eq!(runner.calls(), ["shared:build", "demo:build"]);
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Total: 2"));
        assert!(output.stdout.contains("[ok] shared:build"));
        assert!(output.stdout.contains("[ok] demo:build"));
    }

    #[test]
    fn affected_run_handles_no_selected_projects() {
        let test_workspace = TestWorkspace::new("gomo-affected-command-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        let output = run_with_runner_and_cache(
            test_workspace.path(),
            AffectedRequest {
                target: Target::Build,
                files: vec![PathBuf::from("libs/shared/test/shared_test.gleam")],
                base: None,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect("affected with no selected projects should succeed");

        assert!(runner.calls().is_empty());
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Total: 0"));
    }

    #[test]
    fn affected_run_rejects_empty_files() {
        let test_workspace = TestWorkspace::new("gomo-affected-command-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        let error = run_with_runner_and_cache(
            test_workspace.path(),
            AffectedRequest {
                target: Target::Build,
                files: Vec::new(),
                base: None,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect_err("empty files should fail");

        assert!(
            error
                .to_string()
                .contains("affected requires either --files or --base")
        );
    }

    #[test]
    fn affected_run_does_not_require_git_state() {
        let test_workspace = TestWorkspace::new("gomo-affected-command-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        assert!(!test_workspace.path().join(".git").exists());

        run_with_runner_and_cache(
            test_workspace.path(),
            AffectedRequest {
                target: Target::Build,
                files: vec![PathBuf::from("libs/shared/src/main.gleam")],
                base: None,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect("explicit files should not need VCS state");
    }

    #[test]
    fn affected_run_rejects_files_and_base_together() {
        let test_workspace = TestWorkspace::new("gomo-affected-command-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        let error = run_with_runner_and_cache(
            test_workspace.path(),
            AffectedRequest {
                target: Target::Build,
                files: vec![PathBuf::from("libs/shared/src/main.gleam")],
                base: Some("main".to_string()),
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect_err("mixed affected sources should fail");

        assert!(
            error
                .to_string()
                .contains("affected accepts only one of --files or --base")
        );
    }
}
