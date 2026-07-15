use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache;
use crate::commands::{CommandOutput, OutputOptions};
use crate::graph::ProjectGraph;
use crate::runner::{CommandOptions, CommandRunner, GleamCommandRunner, Target};
use crate::ui;
use crate::workspace::{self, DefaultParallelism, Project, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRequest {
    pub(crate) target: Target,
    pub(crate) command_options: CommandOptions,
    pub(crate) selection: ProjectSelection,
    pub(crate) with_deps: bool,
    pub(crate) parallelism: Parallelism,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheOptions {
    pub(crate) no_cache: bool,
    pub(crate) no_restore: bool,
}

impl CacheOptions {
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            no_cache: true,
            no_restore: true,
        }
    }

    fn should_use_cache(self, target: Target, command_options: CommandOptions) -> bool {
        let cacheable_command = !(target == Target::Format && command_options.format_check);
        target.supports_cache() && cacheable_command && !self.no_cache
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parallelism {
    WorkspaceDefault,
    Auto,
    Fixed(usize),
}

impl Default for Parallelism {
    fn default() -> Self {
        Self::WorkspaceDefault
    }
}

impl Parallelism {
    fn resolve(self, default_parallelism: DefaultParallelism) -> usize {
        match self {
            Self::WorkspaceDefault => match default_parallelism {
                DefaultParallelism::Auto => available_parallelism(),
                DefaultParallelism::Fixed(parallelism) => parallelism,
            },
            Self::Auto => available_parallelism(),
            Self::Fixed(parallelism) => parallelism,
        }
        .max(1)
    }
}

fn available_parallelism() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
}

impl FromStr for Parallelism {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value == "auto" {
            return Ok(Self::Auto);
        }

        let parallelism = value
            .parse::<usize>()
            .map_err(|_| "parallelism must be `auto` or a positive integer".to_string())?;
        if parallelism == 0 {
            return Err("parallelism must be greater than zero".to_string());
        }

        Ok(Self::Fixed(parallelism))
    }
}

impl fmt::Display for Parallelism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceDefault => f.write_str("workspace default"),
            Self::Auto => f.write_str("auto"),
            Self::Fixed(parallelism) => write!(f, "{parallelism}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSelection {
    All,
    Project(String),
    Projects(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Succeeded,
    Failed(i32),
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskCacheStatus {
    Hit,
    Miss,
    Bypassed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskOutcome {
    pub(crate) project: String,
    pub(crate) status: TaskStatus,
    pub(crate) cache_status: Option<TaskCacheStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskGraph {
    dependency_counts: BTreeMap<String, usize>,
    dependents: BTreeMap<String, Vec<String>>,
    order: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedTask {
    output: String,
    outcome: TaskOutcome,
}

struct TaskRunResult {
    project: String,
    result: Result<CompletedTask>,
}

enum TaskRunnerMessage {
    Output { project: String, chunk: String },
    Completed(TaskRunResult),
}

pub(crate) fn run(
    cwd: &Path,
    request: RunRequest,
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

#[cfg(test)]
pub(crate) fn run_with_runner(
    cwd: &Path,
    request: RunRequest,
    runner: &impl CommandRunner,
) -> Result<CommandOutput> {
    run_with_runner_and_cache(
        cwd,
        request,
        runner,
        CacheOptions::disabled(),
        OutputOptions::default(),
    )
}

fn run_with_runner_and_cache(
    cwd: &Path,
    request: RunRequest,
    runner: &(impl CommandRunner + Sync),
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let project_names = selected_project_names(&workspace, &graph, &request)?;

    run_project_names(
        &workspace,
        &graph,
        &project_names,
        request.target,
        request.command_options,
        runner,
        cache_options,
        request.parallelism,
        output_options,
    )
}

pub(crate) fn run_project_names(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project_names: &[String],
    target: Target,
    command_options: CommandOptions,
    runner: &(impl CommandRunner + Sync),
    cache_options: CacheOptions,
    parallelism: Parallelism,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    if !cache_options.no_cache {
        cache::prepare_cache(workspace)?;
    }

    let project_index = project_index(workspace);

    execute_tasks(
        workspace,
        graph,
        project_names,
        &project_index,
        target,
        command_options,
        runner,
        cache_options,
        parallelism,
        output_options,
    )
}

fn selected_project_names(
    workspace: &Workspace,
    graph: &ProjectGraph,
    request: &RunRequest,
) -> Result<Vec<String>> {
    let project_index = project_index(workspace);
    let mut selected = match &request.selection {
        ProjectSelection::All => workspace
            .projects
            .iter()
            .map(|project| project.name.clone())
            .collect::<BTreeSet<_>>(),
        ProjectSelection::Project(project) => BTreeSet::from([project.clone()]),
        ProjectSelection::Projects(projects) => {
            if projects.is_empty() {
                bail!("no projects were selected");
            }
            projects.iter().cloned().collect()
        }
    };

    reject_unknown_projects(workspace, &selected)?;

    if request.with_deps {
        for project in selected.clone() {
            include_upstream_dependencies(
                &project,
                graph,
                &project_index,
                request.target,
                &mut selected,
            );
        }
    }

    Ok(target_topological_order(graph, request.target)
        .iter()
        .filter(|project| selected.contains(*project))
        .cloned()
        .collect())
}

fn include_upstream_dependencies(
    project: &str,
    graph: &ProjectGraph,
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
    selected: &mut BTreeSet<String>,
) {
    for dependency in target_upstream_projects(graph, project_index, project, target) {
        if selected.insert(dependency.clone()) {
            include_upstream_dependencies(&dependency, graph, project_index, target, selected);
        }
    }
}

fn target_topological_order(graph: &ProjectGraph, target: Target) -> &[String] {
    match target {
        Target::Test => &graph.topological_order_with_dev,
        Target::Build | Target::Clean | Target::Format => &graph.topological_order,
    }
}

fn reject_unknown_projects(workspace: &Workspace, selected: &BTreeSet<String>) -> Result<()> {
    let known = workspace
        .projects
        .iter()
        .map(|project| project.name.as_str())
        .collect::<BTreeSet<_>>();

    for project in selected {
        if !known.contains(project.as_str()) {
            let known_projects = known.into_iter().collect::<Vec<_>>().join(", ");
            bail!("unknown project `{project}`. Known projects: {known_projects}");
        }
    }

    Ok(())
}

fn project_index(workspace: &Workspace) -> BTreeMap<&str, &Project> {
    workspace
        .projects
        .iter()
        .map(|project| (project.name.as_str(), project))
        .collect()
}

fn execute_tasks(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project_names: &[String],
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
    command_options: CommandOptions,
    runner: &(impl CommandRunner + Sync),
    cache_options: CacheOptions,
    parallelism: Parallelism,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    execute_tasks_with_hasher(
        workspace,
        graph,
        project_names,
        project_index,
        target,
        command_options,
        runner,
        cache_options,
        parallelism,
        cache::compute_task_hash,
        output_options,
    )
}

fn execute_tasks_with_hasher<R, H>(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project_names: &[String],
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
    command_options: CommandOptions,
    runner: &R,
    cache_options: CacheOptions,
    parallelism: Parallelism,
    compute_task_hash: H,
    output_options: OutputOptions,
) -> Result<CommandOutput>
where
    R: CommandRunner + Sync,
    H: Fn(&Workspace, &ProjectGraph, &Project, Target) -> Result<cache::TaskHash> + Sync,
{
    let task_graph = build_task_graph(graph, project_names, project_index, target);
    let mut remaining_dependencies = task_graph.dependency_counts.clone();
    let mut ready = project_names
        .iter()
        .filter(|project| remaining_dependencies.get(project.as_str()) == Some(&0))
        .cloned()
        .collect::<Vec<_>>();
    let mut task_outputs = BTreeMap::<String, String>::new();
    let mut outcomes_by_project = BTreeMap::<String, TaskOutcome>::new();
    let mut exit_code = 0;
    let mut first_error = None;
    let mut stop_scheduling = false;
    let max_parallel = parallelism.resolve(workspace.default_parallelism);
    let tui_enabled =
        output_options.tui && !output_options.ci && !output_options.json && target.supports_tui();
    let task_commands =
        task_command_displays(project_names, project_index, target, command_options)?;
    let mut terminal = tui_enabled.then(|| {
        ui::run::RunManyTerminal::new(project_names, target, task_commands.clone(), max_parallel)
    });

    if let Some(terminal) = terminal.as_mut() {
        terminal.start()?;
    }

    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<TaskRunnerMessage>();
        let mut running = 0usize;

        loop {
            while !stop_scheduling && first_error.is_none() && running < max_parallel {
                let Some(project_name) = pop_ready_task(&mut ready) else {
                    break;
                };
                let project = project_index
                    .get(project_name.as_str())
                    .expect("selected project exists in project index");
                let command_display = task_commands
                    .get(&project_name)
                    .expect("selected project has command display")
                    .clone();
                let sender = sender.clone();
                let compute_task_hash = &compute_task_hash;
                let started_project = project_name.clone();
                let progress_project = project_name.clone();
                let progress_sender = sender.clone();

                if let Some(terminal) = terminal.as_mut() {
                    if let Err(error) = terminal.task_started(&started_project) {
                        if first_error.is_none() {
                            first_error = Some(error.into());
                        }
                        stop_scheduling = true;
                        break;
                    }
                }
                scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut report_output = |chunk: &str| {
                            if tui_enabled {
                                let _ = progress_sender.send(TaskRunnerMessage::Output {
                                    project: progress_project.clone(),
                                    chunk: chunk.to_string(),
                                });
                            }
                        };
                        execute_single_task(
                            workspace,
                            graph,
                            project,
                            target,
                            command_options,
                            runner,
                            cache_options,
                            compute_task_hash,
                            &command_display,
                            &mut report_output,
                        )
                    }))
                    .unwrap_or_else(|payload| {
                        Err(anyhow::anyhow!(
                            "task worker panicked: {}",
                            panic_payload_message(payload.as_ref())
                        ))
                    });
                    let _ = sender.send(TaskRunnerMessage::Completed(TaskRunResult {
                        project: project_name,
                        result,
                    }));
                });
                running += 1;
            }

            if running == 0 {
                break;
            }

            let task_result = if let Some(terminal) = terminal.as_mut() {
                let mut completed_task = None;
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(TaskRunnerMessage::Completed(task_result)) => {
                            completed_task = Some(task_result);
                            break;
                        }
                        Ok(TaskRunnerMessage::Output { project, chunk }) => {
                            if let Err(error) = terminal.task_output(&project, &chunk) {
                                if first_error.is_none() {
                                    first_error = Some(error.into());
                                }
                                stop_scheduling = true;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(error) = terminal.tick() {
                                if first_error.is_none() {
                                    first_error = Some(error.into());
                                }
                                stop_scheduling = true;
                                continue;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            if first_error.is_none() {
                                first_error = Some(worker_channel_disconnected_error());
                            }
                            stop_scheduling = true;
                            break;
                        }
                    }
                }
                completed_task
            } else {
                match receive_completed_task(&receiver) {
                    Ok(task_result) => Some(task_result),
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        stop_scheduling = true;
                        None
                    }
                }
            };
            let Some(task_result) = task_result else {
                break;
            };
            running -= 1;

            match task_result.result {
                Ok(completed) => {
                    if let Some(terminal) = terminal.as_mut() {
                        if let Err(error) =
                            terminal.task_completed(&completed.outcome, &completed.output)
                        {
                            if first_error.is_none() {
                                first_error = Some(error.into());
                            }
                            stop_scheduling = true;
                        }
                    }
                    let task_succeeded = completed.outcome.status == TaskStatus::Succeeded;
                    if let TaskStatus::Failed(task_exit_code) = &completed.outcome.status {
                        if exit_code == 0 {
                            exit_code = *task_exit_code;
                        }
                    }

                    task_outputs.insert(task_result.project.clone(), completed.output);
                    outcomes_by_project.insert(task_result.project.clone(), completed.outcome);

                    if task_succeeded {
                        for dependent in task_graph
                            .dependents
                            .get(&task_result.project)
                            .into_iter()
                            .flatten()
                        {
                            let count = remaining_dependencies
                                .get_mut(dependent)
                                .expect("dependent project exists in dependency count map");
                            *count -= 1;
                            if *count == 0 {
                                insert_ready_task(&mut ready, dependent.clone(), &task_graph.order);
                            }
                        }
                    }
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    stop_scheduling = true;
                }
            }
        }
    });

    if let Some(error) = first_error {
        if let Some(terminal) = terminal.as_mut() {
            let _ = terminal.abort();
        }
        return Err(error);
    }

    if exit_code == 0 && outcomes_by_project.len() != project_names.len() {
        if let Some(terminal) = terminal.as_mut() {
            let _ = terminal.abort();
        }
        bail!("task scheduler stalled before all selected tasks completed");
    }

    for project_name in project_names {
        if !outcomes_by_project.contains_key(project_name) {
            let outcome = TaskOutcome {
                project: project_name.clone(),
                status: TaskStatus::Skipped,
                cache_status: None,
            };
            if let Some(terminal) = terminal.as_mut() {
                terminal.task_skipped(&outcome)?;
            }
            outcomes_by_project.insert(project_name.clone(), outcome);
        }
    }

    let outcomes = project_names
        .iter()
        .filter_map(|project| outcomes_by_project.get(project).cloned())
        .collect::<Vec<_>>();
    let summary = summarize_tasks(target, &outcomes);
    let terminal_exit = if let Some(terminal) = terminal.as_mut() {
        Some(terminal.finish(&summary)?)
    } else {
        None
    };
    let mut output = String::new();
    if output_options.json {
        output.push_str(&render_json_summary(&summary)?);
    } else if tui_enabled {
        if terminal_exit == Some(ui::run::RunManyExit::AutoExited) {
            output.push_str(&render_auto_exit_logs(
                &summary,
                project_names,
                &mut task_outputs,
            ));
            output.push_str(&render_summary(&summary));
        } else {
            output.push_str(&ui::run::render_terminal_summary(&summary));
        }
    } else if !tui_enabled {
        append_task_outputs(
            &mut output,
            project_names,
            &mut task_outputs,
            output_options.ci,
        );
        output.push_str(&render_summary(&summary));
    }

    Ok(CommandOutput::with_exit_code(output, exit_code))
}

fn receive_completed_task(receiver: &mpsc::Receiver<TaskRunnerMessage>) -> Result<TaskRunResult> {
    loop {
        match receiver.recv() {
            Ok(TaskRunnerMessage::Completed(task_result)) => return Ok(task_result),
            Ok(TaskRunnerMessage::Output { .. }) => {}
            Err(_) => return Err(worker_channel_disconnected_error()),
        }
    }
}

fn worker_channel_disconnected_error() -> anyhow::Error {
    anyhow::anyhow!("task worker channel disconnected before reporting completion")
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn task_command_displays(
    project_names: &[String],
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
    command_options: CommandOptions,
) -> Result<BTreeMap<String, String>> {
    project_names
        .iter()
        .map(|project_name| {
            let project = project_index
                .get(project_name.as_str())
                .with_context(|| format!("unknown project `{project_name}`"))?;
            Ok((
                project_name.clone(),
                command_options.command_display(project, target)?,
            ))
        })
        .collect()
}

fn build_task_graph(
    graph: &ProjectGraph,
    project_names: &[String],
    project_index: &BTreeMap<&str, &Project>,
    target: Target,
) -> TaskGraph {
    let selected = project_names.iter().cloned().collect::<BTreeSet<_>>();
    let order = project_names
        .iter()
        .enumerate()
        .map(|(index, project)| (project.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut dependency_counts = BTreeMap::new();
    let mut dependents = project_names
        .iter()
        .map(|project| (project.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();

    for project in project_names {
        let selected_dependencies = target_upstream_projects(graph, project_index, project, target)
            .into_iter()
            .filter(|dependency| selected.contains(dependency.as_str()))
            .collect::<Vec<_>>();
        dependency_counts.insert(project.clone(), selected_dependencies.len());

        for dependency in selected_dependencies {
            dependents
                .get_mut(&dependency)
                .expect("selected dependency exists in dependents map")
                .push(project.clone());
        }
    }

    for project_dependents in dependents.values_mut() {
        project_dependents.sort_by_key(|project| order.get(project).copied().unwrap_or(usize::MAX));
    }

    TaskGraph {
        dependency_counts,
        dependents,
        order,
    }
}

fn target_upstream_projects(
    graph: &ProjectGraph,
    project_index: &BTreeMap<&str, &Project>,
    project: &str,
    target: Target,
) -> Vec<String> {
    match target {
        Target::Test => graph.upstream_with_dev_for(project),
        Target::Build if uses_custom_build_command(project_index, project) => {
            graph.upstream_with_dev_for(project)
        }
        Target::Build | Target::Clean | Target::Format => graph.upstream_for(project).to_vec(),
    }
}

fn uses_custom_build_command(project_index: &BTreeMap<&str, &Project>, project: &str) -> bool {
    project_index
        .get(project)
        .and_then(|project| project.gomo_targets.get(Target::Build.as_str()))
        .and_then(|config| config.command.as_ref())
        .is_some()
}

fn pop_ready_task(ready: &mut Vec<String>) -> Option<String> {
    if ready.is_empty() {
        None
    } else {
        Some(ready.remove(0))
    }
}

fn insert_ready_task(ready: &mut Vec<String>, project: String, order: &BTreeMap<String, usize>) {
    if ready.contains(&project) {
        return;
    }

    ready.push(project);
    ready.sort_by_key(|project| order.get(project).copied().unwrap_or(usize::MAX));
}

fn execute_single_task<R, H>(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project: &Project,
    target: Target,
    command_options: CommandOptions,
    runner: &R,
    cache_options: CacheOptions,
    compute_task_hash: &H,
    command_display: &str,
    output_progress: &mut dyn FnMut(&str),
) -> Result<CompletedTask>
where
    R: CommandRunner + Sync,
    H: Fn(&Workspace, &ProjectGraph, &Project, Target) -> Result<cache::TaskHash> + Sync,
{
    let mut output = String::new();
    output.push_str(&format!(
        "==> {}:{} ({})\n",
        project.name,
        target,
        project.root_relative_path.display()
    ));
    output.push_str(&format!("$ {command_display}\n"));

    let task_hash = if cache_options.should_use_cache(target, command_options) {
        let task_hash = compute_task_hash(workspace, graph, project, target)?;
        if cache_options.no_restore {
            output.push_str(&format!("[cache restore disabled] {}\n", task_hash.hash));
        } else if let Some(cached_execution) = restore_cached_task(workspace, project, &task_hash)?
        {
            output.push_str(&format!("[cache hit] {}\n", task_hash.hash));
            append_stream(&mut output, &cached_execution.stdout);
            append_stream(&mut output, &cached_execution.stderr);
            output.push('\n');
            return Ok(CompletedTask {
                output,
                outcome: TaskOutcome {
                    project: project.name.clone(),
                    status: TaskStatus::Succeeded,
                    cache_status: Some(TaskCacheStatus::Hit),
                },
            });
        } else {
            output.push_str(&format!("[cache miss] {}\n", task_hash.hash));
        }

        Some(task_hash)
    } else {
        None
    };

    let execution = runner.run_with_output(project, target, command_options, output_progress);
    append_stream(&mut output, &execution.stdout);
    append_stream(&mut output, &execution.stderr);

    let cache_status = task_hash.as_ref().map(|_| {
        if cache_options.no_restore {
            TaskCacheStatus::Bypassed
        } else {
            TaskCacheStatus::Miss
        }
    });

    let status = if execution.is_success() {
        if let Some(task_hash) = &task_hash {
            let task_hash_for_store = if target.refreshes_cache_key_after_success() {
                compute_task_hash(workspace, graph, project, target)
            } else {
                Ok(task_hash.clone())
            };

            match task_hash_for_store {
                Ok(task_hash_for_store) => {
                    if task_hash_for_store.hash != task_hash.hash {
                        output.push_str(&format!(
                            "[cache key updated] {}\n",
                            task_hash_for_store.hash
                        ));
                    }
                    match store_successful_task(
                        workspace,
                        project,
                        &task_hash_for_store,
                        &execution,
                    ) {
                        Ok(()) => output.push_str("[cache stored]\n"),
                        Err(error) => output.push_str(&format!("[cache store failed] {error}\n")),
                    }
                }
                Err(error) => output.push_str(&format!("[cache store failed] {error}\n")),
            }
        }

        if target == Target::Clean && !cache_options.no_cache {
            match cache::remove_project_build_cache(workspace, project) {
                Ok(true) => output.push_str("[build cache removed]\n"),
                Ok(false) => {}
                Err(error) => output.push_str(&format!("[build cache remove failed] {error}\n")),
            }
        }

        TaskStatus::Succeeded
    } else {
        TaskStatus::Failed(execution.exit_code)
    };

    output.push('\n');
    Ok(CompletedTask {
        output,
        outcome: TaskOutcome {
            project: project.name.clone(),
            status,
            cache_status,
        },
    })
}

fn restore_cached_task(
    workspace: &Workspace,
    project: &Project,
    task_hash: &cache::TaskHash,
) -> Result<Option<cache::CachedTaskExecution>> {
    match task_hash.target {
        Target::Build => cache::restore_successful_build(workspace, project, task_hash),
        Target::Format => cache::restore_successful_format(workspace, task_hash),
        Target::Test => cache::restore_successful_test(workspace, task_hash),
        Target::Clean => Ok(None),
    }
}

fn store_successful_task(
    workspace: &Workspace,
    project: &Project,
    task_hash: &cache::TaskHash,
    execution: &crate::runner::TaskExecution,
) -> Result<()> {
    match task_hash.target {
        Target::Build => cache::store_successful_build(workspace, project, task_hash, execution),
        Target::Format => cache::store_successful_format(workspace, project, task_hash, execution),
        Target::Test => cache::store_successful_test(workspace, project, task_hash, execution),
        Target::Clean => Ok(()),
    }
}

fn append_stream(output: &mut String, stream: &str) {
    if stream.is_empty() {
        return;
    }

    output.push_str(stream);
    if !stream.ends_with('\n') {
        output.push('\n');
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskSummary {
    pub(crate) target: Target,
    pub(crate) total: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
    pub(crate) cache_hits: usize,
    pub(crate) cache_misses: usize,
    pub(crate) cache_bypassed: usize,
    pub(crate) outcomes: Vec<TaskOutcome>,
}

fn summarize_tasks(target: Target, outcomes: &[TaskOutcome]) -> TaskSummary {
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.status == TaskStatus::Succeeded)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, TaskStatus::Failed(_)))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|outcome| outcome.status == TaskStatus::Skipped)
        .count();
    let cache_hits = outcomes
        .iter()
        .filter(|outcome| outcome.cache_status == Some(TaskCacheStatus::Hit))
        .count();
    let cache_misses = outcomes
        .iter()
        .filter(|outcome| outcome.cache_status == Some(TaskCacheStatus::Miss))
        .count();
    let cache_bypassed = outcomes
        .iter()
        .filter(|outcome| outcome.cache_status == Some(TaskCacheStatus::Bypassed))
        .count();

    TaskSummary {
        target,
        total: outcomes.len(),
        succeeded,
        failed,
        skipped,
        cache_hits,
        cache_misses,
        cache_bypassed,
        outcomes: outcomes.to_vec(),
    }
}

fn render_summary(summary: &TaskSummary) -> String {
    let mut output = format!(
        "Task Summary\nTarget: {target}\nTotal: {}\nSucceeded: {succeeded}\nFailed: {failed}\nSkipped: {skipped}\n",
        summary.total,
        target = summary.target,
        succeeded = summary.succeeded,
        failed = summary.failed,
        skipped = summary.skipped,
    );

    if summary.cache_hits + summary.cache_misses + summary.cache_bypassed > 0 {
        output.push_str(&format!(
            "Cache Hits: {cache_hits}\nCache Misses: {cache_misses}\nCache Bypassed: {cache_bypassed}\n"
            ,
            cache_hits = summary.cache_hits,
            cache_misses = summary.cache_misses,
            cache_bypassed = summary.cache_bypassed,
        ));
    }

    for outcome in &summary.outcomes {
        let status = match &outcome.status {
            TaskStatus::Succeeded if outcome.cache_status == Some(TaskCacheStatus::Hit) => {
                "cached".to_string()
            }
            TaskStatus::Succeeded => "ok".to_string(),
            TaskStatus::Failed(exit_code) => format!("failed ({exit_code})"),
            TaskStatus::Skipped => "skipped".to_string(),
        };
        output.push_str(&format!(
            "[{status}] {}:{}\n",
            outcome.project, summary.target
        ));
    }

    output
}

fn render_auto_exit_logs(
    summary: &TaskSummary,
    project_names: &[String],
    task_outputs: &mut BTreeMap<String, String>,
) -> String {
    let mut output = format!(
        "Gomo auto-exited after completing target `{}`. Captured task logs follow.\nTarget: {}\nTotal Tasks: {}\n\n--- BEGIN GOMO TASK LOGS ---\n",
        summary.target, summary.target, summary.total,
    );
    append_task_outputs(&mut output, project_names, task_outputs, false);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("--- END GOMO TASK LOGS ---\n");
    output
}

fn append_task_outputs(
    output: &mut String,
    project_names: &[String],
    task_outputs: &mut BTreeMap<String, String>,
    plain: bool,
) {
    for project_name in project_names {
        if let Some(task_output) = task_outputs.remove(project_name) {
            if plain {
                output.push_str(&strip_terminal_sequences(&task_output));
            } else {
                output.push_str(&task_output);
            }
        }
    }
}

fn strip_terminal_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(character) = chars.next() {
        if character != '\x1b' {
            output.push(character);
            continue;
        }

        match chars.next() {
            Some('[') => {
                for character in chars.by_ref() {
                    if ('@'..='~').contains(&character) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut escape = false;
                for character in chars.by_ref() {
                    if character == '\x07' || (escape && character == '\\') {
                        break;
                    }
                    escape = character == '\x1b';
                }
            }
            Some(_) | None => {}
        }
    }

    output
}

fn render_json_summary(summary: &TaskSummary) -> Result<String> {
    let output = TaskSummaryJson::from(summary);
    let mut json = serde_json::to_string_pretty(&output).context("failed to serialize run JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct TaskSummaryJson<'a> {
    target: &'static str,
    total: usize,
    succeeded: usize,
    failed: usize,
    skipped: usize,
    cache_hits: usize,
    cache_misses: usize,
    cache_bypassed: usize,
    tasks: Vec<TaskOutcomeJson<'a>>,
}

#[derive(Serialize)]
struct TaskOutcomeJson<'a> {
    project: &'a str,
    target: &'static str,
    status: &'static str,
    exit_code: Option<i32>,
    cache: Option<&'static str>,
}

impl<'a> From<&'a TaskSummary> for TaskSummaryJson<'a> {
    fn from(summary: &'a TaskSummary) -> Self {
        Self {
            target: summary.target.as_str(),
            total: summary.total,
            succeeded: summary.succeeded,
            failed: summary.failed,
            skipped: summary.skipped,
            cache_hits: summary.cache_hits,
            cache_misses: summary.cache_misses,
            cache_bypassed: summary.cache_bypassed,
            tasks: summary
                .outcomes
                .iter()
                .map(|outcome| TaskOutcomeJson {
                    project: &outcome.project,
                    target: summary.target.as_str(),
                    status: outcome.status.as_str(),
                    exit_code: outcome.status.exit_code(),
                    cache: outcome.cache_status.map(TaskCacheStatus::as_str),
                })
                .collect(),
        }
    }
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Failed(exit_code) => Some(*exit_code),
            Self::Succeeded | Self::Skipped => None,
        }
    }
}

impl TaskCacheStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypassed => "bypassed",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use super::*;
    use crate::runner::TaskExecution;
    use crate::test_support::TestWorkspace;

    const GLEAM_VERSION: &str = "gleam 1.0.0";

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<String>>,
        failures: BTreeMap<String, i32>,
    }

    impl FakeRunner {
        fn failing(project: &str, exit_code: i32) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failures: BTreeMap::from([(project.to_string(), exit_code)]),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, project: &Project, target: Target, options: CommandOptions) -> TaskExecution {
            let target_display = if options.format_check && target == Target::Format {
                format!("{target} --check")
            } else {
                target.to_string()
            };
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .push(format!("{}:{target_display}", project.name));

            if let Some(exit_code) = self.failures.get(&project.name) {
                return TaskExecution::failure(
                    *exit_code,
                    format!("{} failed\n", project.name),
                    "",
                );
            }

            TaskExecution::success(format!("{} passed\n", project.name), "")
        }
    }

    #[derive(Default)]
    struct FormattingRunner {
        calls: Mutex<Vec<String>>,
    }

    impl FormattingRunner {
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .clone()
        }
    }

    impl CommandRunner for FormattingRunner {
        fn run(
            &self,
            project: &Project,
            target: Target,
            _options: CommandOptions,
        ) -> TaskExecution {
            self.calls
                .lock()
                .expect("calls lock should not be poisoned")
                .push(format!("{}:{target}", project.name));
            fs::write(
                project.root.join("src/main.gleam"),
                "pub fn main() { Nil }\n",
            )
            .expect("format runner should update source");

            TaskExecution::success("formatted\n", "")
        }
    }

    #[derive(Default)]
    struct ConcurrencyRunner {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ConcurrencyRunner {
        fn max_active(&self) -> usize {
            self.max_active.load(Ordering::SeqCst)
        }

        fn record_active(&self, active: usize) {
            let mut observed = self.max_active.load(Ordering::SeqCst);
            while active > observed {
                match self.max_active.compare_exchange(
                    observed,
                    active,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(current) => observed = current,
                }
            }
        }
    }

    impl CommandRunner for ConcurrencyRunner {
        fn run(
            &self,
            project: &Project,
            _target: Target,
            _options: CommandOptions,
        ) -> TaskExecution {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.record_active(active);
            std::thread::sleep(Duration::from_millis(50));
            self.active.fetch_sub(1, Ordering::SeqCst);

            TaskExecution::success(format!("{} passed\n", project.name), "")
        }
    }

    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(
            &self,
            _project: &Project,
            _target: Target,
            _options: CommandOptions,
        ) -> TaskExecution {
            panic!("runner panic")
        }
    }

    fn compute_hash_with_fixed_gleam_version(
        workspace: &Workspace,
        graph: &ProjectGraph,
        project: &Project,
        target: Target,
    ) -> Result<cache::TaskHash> {
        cache::compute_task_hash_with_gleam_version(
            workspace,
            graph,
            project,
            target,
            GLEAM_VERSION,
        )
    }

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

    fn store_protocol_build_cache(test_workspace: &TestWorkspace) -> std::path::PathBuf {
        test_workspace.write_file("libs/protocol/src/main.gleam", "pub fn value() { 1 }\n");
        test_workspace.write_file(
            "libs/protocol/build/dev/erlang/protocol/_gleam_artefacts/protocol.erl",
            "compiled\n",
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let protocol = workspace
            .projects
            .iter()
            .find(|project| project.name == "protocol")
            .expect("protocol project should exist");
        let task_hash = cache::compute_task_hash_with_gleam_version(
            &workspace,
            &graph,
            protocol,
            Target::Build,
            GLEAM_VERSION,
        )
        .expect("build hash should compute");
        let entry_dir = cache::task_cache_entry_dir(&workspace, &task_hash);
        cache::store_successful_build(
            &workspace,
            protocol,
            &task_hash,
            &TaskExecution::success("built\n", ""),
        )
        .expect("build cache should be stored");

        assert!(entry_dir.is_dir());
        entry_dir
    }

    fn write_independent_fixture(test_workspace: &TestWorkspace) {
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/alpha",
            r#"
name = "alpha"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "libs/beta",
            r#"
name = "beta"
version = "0.1.0"
"#,
        );
    }

    #[test]
    fn run_many_all_executes_in_topological_order() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("tasks should run");

        assert_eq!(
            runner.calls(),
            ["protocol:build", "renderer:build", "demo:build"]
        );
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Task Summary"));
        assert!(output.stdout.contains("Succeeded: 3"));
        assert!(output.stdout.contains("[ok] demo:build"));
    }

    #[test]
    fn run_all_schedules_path_dependencies_outside_project_roots() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
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
        let runner = FakeRunner::default();

        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("tasks should run");

        assert_eq!(runner.calls(), ["esgleam:build", "demo:build"]);
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Total: 2"));
    }

    #[test]
    fn run_can_render_json_summary() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        let output = run_with_runner_and_cache(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions::disabled(),
            OutputOptions {
                json: true,
                ci: true,
                tui: false,
                terminal_width: None,
            },
        )
        .expect("JSON run should succeed");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert_eq!(value["target"], "build");
        assert_eq!(value["total"], 1);
        assert_eq!(value["tasks"][0]["project"], "protocol");
        assert_eq!(value["tasks"][0]["status"], "succeeded");
        assert!(!output.stdout.contains("Task Summary"));
    }

    #[test]
    fn auto_exit_log_dump_wraps_captured_task_outputs() {
        let summary = TaskSummary {
            target: Target::Build,
            total: 1,
            succeeded: 1,
            failed: 0,
            skipped: 0,
            cache_hits: 0,
            cache_misses: 1,
            cache_bypassed: 0,
            outcomes: vec![TaskOutcome {
                project: "protocol".to_string(),
                status: TaskStatus::Succeeded,
                cache_status: Some(TaskCacheStatus::Miss),
            }],
        };
        let project_names = vec!["protocol".to_string()];
        let mut task_outputs = BTreeMap::from([(
            "protocol".to_string(),
            "==> protocol:build (libs/protocol)\n$ gleam build\ncompiled\n\n".to_string(),
        )]);

        let output = render_auto_exit_logs(&summary, &project_names, &mut task_outputs);

        assert!(output.contains("Gomo auto-exited after completing target `build`"));
        assert!(output.contains("--- BEGIN GOMO TASK LOGS ---"));
        assert!(output.contains("==> protocol:build (libs/protocol)"));
        assert!(output.contains("compiled"));
        assert!(output.contains("--- END GOMO TASK LOGS ---"));
        assert!(task_outputs.is_empty());
    }

    #[test]
    fn plain_task_output_strips_terminal_sequences() {
        let project_names = vec!["protocol".to_string()];
        let mut task_outputs = BTreeMap::from([(
            "protocol".to_string(),
            "\x1b[32mcompiled\x1b[39m\n\x1b]0;task title\x07done\n".to_string(),
        )]);
        let mut output = String::new();

        append_task_outputs(&mut output, &project_names, &mut task_outputs, true);

        assert_eq!(output, "compiled\ndone\n");
        assert!(task_outputs.is_empty());
    }

    #[test]
    fn receive_completed_task_reports_disconnected_channel() {
        let (sender, receiver) = mpsc::channel::<TaskRunnerMessage>();
        drop(sender);

        let error = match receive_completed_task(&receiver) {
            Ok(_) => panic!("disconnected channel should fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("task worker channel disconnected")
        );
    }

    #[test]
    fn runner_panic_returns_error_instead_of_unwinding() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_independent_fixture(&test_workspace);

        let error = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("alpha".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &PanicRunner,
        )
        .expect_err("runner panic should become an error");

        assert!(error.to_string().contains("task worker panicked"));
        assert!(error.to_string().contains("runner panic"));
    }

    #[test]
    fn independent_projects_run_concurrently() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_independent_fixture(&test_workspace);
        let runner = ConcurrencyRunner::default();
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("independent tasks should run");

        assert_eq!(output.exit_code, 0);
        assert!(
            runner.max_active() >= 2,
            "expected at least two tasks to run concurrently"
        );
    }

    #[test]
    fn workspace_default_parallelism_limits_concurrency() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]
default_parallelism = "1"
"#,
        );
        test_workspace.write_manifest(
            "libs/alpha",
            r#"
name = "alpha"
version = "0.1.0"
"#,
        );
        test_workspace.write_manifest(
            "libs/beta",
            r#"
name = "beta"
version = "0.1.0"
"#,
        );
        let runner = ConcurrencyRunner::default();

        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::WorkspaceDefault,
            },
            &runner,
        )
        .expect("tasks should run");

        assert_eq!(output.exit_code, 0);
        assert_eq!(runner.max_active(), 1);
    }

    #[test]
    fn run_with_deps_expands_upstream_projects() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Test,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("demo".to_string()),
                with_deps: true,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("tasks should run");

        assert_eq!(
            runner.calls(),
            ["protocol:test", "renderer:test", "demo:test"]
        );
    }

    #[test]
    fn run_without_deps_executes_only_selected_project() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();

        run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("demo".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("task should run");

        assert_eq!(runner.calls(), ["demo:build"]);
    }

    #[test]
    fn default_build_with_deps_does_not_expand_dev_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
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
        let runner = FakeRunner::default();

        run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("demo".to_string()),
                with_deps: true,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("build task should run");

        assert_eq!(runner.calls(), ["demo:build"]);
    }

    #[test]
    fn custom_build_with_deps_expands_dev_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
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
        let runner = FakeRunner::default();

        run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("demo".to_string()),
                with_deps: true,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("custom build task should run");

        assert_eq!(runner.calls(), ["test_support:build", "demo:build"]);
    }

    #[test]
    fn clean_target_runs_gleam_clean() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Clean,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("clean task should run");

        assert_eq!(runner.calls(), ["protocol:clean"]);
        assert!(output.stdout.contains("$ gleam clean"));
        assert!(output.stdout.contains("[ok] protocol:clean"));
    }

    #[test]
    fn successful_clean_removes_project_build_cache_entries() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let entry_dir = store_protocol_build_cache(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner_and_cache(
            test_workspace.path(),
            RunRequest {
                target: Target::Clean,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions {
                no_cache: false,
                no_restore: false,
            },
            OutputOptions::default(),
        )
        .expect("clean task should run");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("[build cache removed]"));
        assert!(!entry_dir.exists());
    }

    #[test]
    fn no_cache_clean_keeps_project_build_cache_entries() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let entry_dir = store_protocol_build_cache(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner_and_cache(
            test_workspace.path(),
            RunRequest {
                target: Target::Clean,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions {
                no_cache: true,
                no_restore: true,
            },
            OutputOptions::default(),
        )
        .expect("clean task should run");

        assert_eq!(output.exit_code, 0);
        assert!(!output.stdout.contains("[build cache removed]"));
        assert!(entry_dir.is_dir());
    }

    #[test]
    fn failed_clean_keeps_project_build_cache_entries() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let entry_dir = store_protocol_build_cache(&test_workspace);
        let runner = FakeRunner::failing("protocol", 7);
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Clean,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("clean task failure should still return output");

        assert_eq!(output.exit_code, 7);
        assert!(!output.stdout.contains("[build cache removed]"));
        assert!(entry_dir.is_dir());
    }

    #[test]
    fn format_target_runs_gleam_format() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Format,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("format task should run");

        assert_eq!(runner.calls(), ["protocol:format"]);
        assert!(output.stdout.contains("$ gleam format"));
        assert!(output.stdout.contains("[ok] protocol:format"));
    }

    #[test]
    fn format_check_runs_gleam_format_check_without_cache() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        let output = run_with_runner_and_cache(
            test_workspace.path(),
            RunRequest {
                target: Target::Format,
                command_options: CommandOptions { format_check: true },
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions {
                no_cache: false,
                no_restore: false,
            },
            OutputOptions::default(),
        )
        .expect("format check task should run");

        assert_eq!(runner.calls(), ["protocol:format --check"]);
        assert!(output.stdout.contains("$ gleam format --check"));
        assert!(!output.stdout.contains("[cache"));
        assert!(output.stdout.contains("[ok] protocol:format"));
    }

    #[test]
    fn custom_test_command_is_rendered_in_task_output() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/protocol",
            r#"
name = "protocol"
version = "0.1.0"

[tools.gomo.test]
command = "gleam test --target erlang"
"#,
        );
        let runner = FakeRunner::default();

        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Test,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("custom test command should run");

        assert_eq!(runner.calls(), ["protocol:test"]);
        assert!(output.stdout.contains("$ gleam test --target erlang"));
        assert!(output.stdout.contains("[ok] protocol:test"));
    }

    #[test]
    fn custom_format_check_command_is_rendered_in_task_output() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/protocol",
            r#"
name = "protocol"
version = "0.1.0"

[tools.gomo.format]
command = "mise exec -- gleam format"

[tools.gomo.format.check]
command = "mise exec -- gleam format --check"
"#,
        );
        let runner = FakeRunner::default();

        let output = run_with_runner_and_cache(
            test_workspace.path(),
            RunRequest {
                target: Target::Format,
                command_options: CommandOptions { format_check: true },
                selection: ProjectSelection::Project("protocol".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
            CacheOptions {
                no_cache: false,
                no_restore: false,
            },
            OutputOptions::default(),
        )
        .expect("custom format check command should run");

        assert_eq!(runner.calls(), ["protocol:format --check"]);
        assert!(
            output
                .stdout
                .contains("$ mise exec -- gleam format --check")
        );
        assert!(!output.stdout.contains("[cache"));
        assert!(output.stdout.contains("[ok] protocol:format"));
    }

    #[test]
    fn format_target_reuses_cache_after_formatting_changes_inputs() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        test_workspace.write_file("libs/protocol/src/main.gleam", "pub fn main(){Nil}\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project_names = vec!["protocol".to_string()];
        let project_index = project_index(&workspace);
        let runner = FormattingRunner::default();
        let cache_options = CacheOptions {
            no_cache: false,
            no_restore: false,
        };

        let first = execute_tasks_with_hasher(
            &workspace,
            &graph,
            &project_names,
            &project_index,
            Target::Format,
            CommandOptions::default(),
            &runner,
            cache_options,
            Parallelism::Fixed(1),
            compute_hash_with_fixed_gleam_version,
            OutputOptions::default(),
        )
        .expect("format task should run");

        assert_eq!(runner.calls(), ["protocol:format"]);
        assert!(first.stdout.contains("[cache miss]"));
        assert!(first.stdout.contains("[cache key updated]"));
        assert!(first.stdout.contains("[cache stored]"));
        assert!(first.stdout.contains("Cache Misses: 1"));

        let second = execute_tasks_with_hasher(
            &workspace,
            &graph,
            &project_names,
            &project_index,
            Target::Format,
            CommandOptions::default(),
            &runner,
            cache_options,
            Parallelism::Fixed(1),
            compute_hash_with_fixed_gleam_version,
            OutputOptions::default(),
        )
        .expect("cached format task should run");

        assert_eq!(runner.calls(), ["protocol:format"]);
        assert!(second.stdout.contains("[cache hit]"));
        assert!(second.stdout.contains("formatted"));
        assert!(second.stdout.contains("Cache Hits: 1"));
        assert!(second.stdout.contains("[cached] protocol:format"));
    }

    #[test]
    fn test_target_reuses_successful_cache_entries() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        test_workspace.write_file(
            "libs/protocol/test/protocol_test.gleam",
            "pub fn protocol_test() { Nil }\n",
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project_names = vec!["protocol".to_string()];
        let project_index = project_index(&workspace);
        let runner = FakeRunner::default();
        let cache_options = CacheOptions {
            no_cache: false,
            no_restore: false,
        };

        let first = execute_tasks_with_hasher(
            &workspace,
            &graph,
            &project_names,
            &project_index,
            Target::Test,
            CommandOptions::default(),
            &runner,
            cache_options,
            Parallelism::Fixed(1),
            compute_hash_with_fixed_gleam_version,
            OutputOptions::default(),
        )
        .expect("test task should run");

        assert_eq!(runner.calls(), ["protocol:test"]);
        assert!(first.stdout.contains("[cache miss]"));
        assert!(first.stdout.contains("[cache stored]"));
        assert!(first.stdout.contains("Cache Misses: 1"));

        let second = execute_tasks_with_hasher(
            &workspace,
            &graph,
            &project_names,
            &project_index,
            Target::Test,
            CommandOptions::default(),
            &runner,
            cache_options,
            Parallelism::Fixed(1),
            compute_hash_with_fixed_gleam_version,
            OutputOptions::default(),
        )
        .expect("cached test task should run");

        assert_eq!(runner.calls(), ["protocol:test"]);
        assert!(second.stdout.contains("[cache hit]"));
        assert!(second.stdout.contains("protocol passed"));
        assert!(second.stdout.contains("Cache Hits: 1"));
        assert!(second.stdout.contains("[cached] protocol:test"));
    }

    #[test]
    fn failed_test_runs_are_not_cached() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        test_workspace.write_file(
            "libs/protocol/test/protocol_test.gleam",
            "pub fn protocol_test() { Nil }\n",
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project_names = vec!["protocol".to_string()];
        let project_index = project_index(&workspace);
        let protocol = project_index
            .get("protocol")
            .expect("project should exist in index");
        let runner = FakeRunner::failing("protocol", 9);
        let cache_options = CacheOptions {
            no_cache: false,
            no_restore: false,
        };

        let output = execute_tasks_with_hasher(
            &workspace,
            &graph,
            &project_names,
            &project_index,
            Target::Test,
            CommandOptions::default(),
            &runner,
            cache_options,
            Parallelism::Fixed(1),
            compute_hash_with_fixed_gleam_version,
            OutputOptions::default(),
        )
        .expect("test task failure should still return output");
        let task_hash =
            compute_hash_with_fixed_gleam_version(&workspace, &graph, protocol, Target::Test)
                .expect("test hash should compute");

        assert_eq!(runner.calls(), ["protocol:test"]);
        assert_eq!(output.exit_code, 9);
        assert!(output.stdout.contains("[cache miss]"));
        assert!(!output.stdout.contains("[cache stored]"));
        assert!(
            cache::restore_successful_test(&workspace, &task_hash)
                .expect("cache lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn skips_dependents_after_failure_and_summarizes_tasks() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::failing("renderer", 42);
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect("task failure should still return command output");

        assert_eq!(runner.calls(), ["protocol:build", "renderer:build"]);
        assert_eq!(output.exit_code, 42);
        assert!(output.stdout.contains("Failed: 1"));
        assert!(output.stdout.contains("Skipped: 1"));
        assert!(output.stdout.contains("[failed (42)] renderer:build"));
        assert!(output.stdout.contains("[skipped] demo:build"));
    }

    #[test]
    fn continues_scheduling_independent_tasks_after_failure() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_independent_fixture(&test_workspace);
        let runner = FakeRunner::failing("alpha", 42);
        let output = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::All,
                with_deps: false,
                parallelism: Parallelism::Fixed(1),
            },
            &runner,
        )
        .expect("task failure should still return command output");

        assert_eq!(runner.calls(), ["alpha:build", "beta:build"]);
        assert_eq!(output.exit_code, 42);
        assert!(output.stdout.contains("Succeeded: 1"));
        assert!(output.stdout.contains("Failed: 1"));
        assert!(output.stdout.contains("Skipped: 0"));
        assert!(output.stdout.contains("[failed (42)] alpha:build"));
        assert!(output.stdout.contains("[ok] beta:build"));
    }

    #[test]
    fn rejects_unknown_projects() {
        let test_workspace = TestWorkspace::new("gomo-run-test");
        write_graph_fixture(&test_workspace);
        let runner = FakeRunner::default();
        let error = run_with_runner(
            test_workspace.path(),
            RunRequest {
                target: Target::Build,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project("missing".to_string()),
                with_deps: false,
                parallelism: Parallelism::Fixed(2),
            },
            &runner,
        )
        .expect_err("unknown project should fail");

        assert!(error.to_string().contains("unknown project `missing`"));
        assert!(error.to_string().contains("Known projects:"));
    }
}
