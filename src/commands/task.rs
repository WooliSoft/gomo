use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    static TARGET_TASK_STACK: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

struct TargetTaskStackReset(Vec<(String, String)>);

impl Drop for TargetTaskStackReset {
    fn drop(&mut self) {
        TARGET_TASK_STACK.with(|stack| {
            stack.replace(std::mem::take(&mut self.0));
        });
    }
}

pub(crate) fn target_task_stack_contains(key: &(String, String)) -> bool {
    TARGET_TASK_STACK.with(|stack| stack.borrow().iter().any(|entry| entry == key))
}

pub(crate) fn target_task_stack() -> Vec<(String, String)> {
    TARGET_TASK_STACK.with(|stack| stack.borrow().clone())
}

pub(crate) fn with_target_task_stack<T>(
    stack: Vec<(String, String)>,
    operation: impl FnOnce() -> T,
) -> T {
    TARGET_TASK_STACK.with(|target_task_stack| {
        let previous = target_task_stack.replace(stack);
        let _reset = TargetTaskStackReset(previous);
        operation()
    })
}

pub(crate) fn with_pushed_target_task<T>(
    key: (String, String),
    operation: impl FnOnce() -> T,
) -> T {
    let mut stack = target_task_stack();
    stack.push(key);
    with_target_task_stack(stack, operation)
}

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use walkdir::WalkDir;

use crate::cache;
use crate::cancellation::CancellationToken;
use crate::commands::{CommandOutput, OutputOptions};
use crate::remote_cache::RemoteCacheClient;
use crate::runner::{CommandOptions, Target};
use crate::task::{
    DependencyCacheStrategy, ExecAction, ShellAction, Task, TaskMode, TaskScope, TaskStep,
};
use crate::ui::run::RunManyTerminal;
use crate::ui::surface::{self, RenderSurface, SurfaceEvent};
use crate::workspace::{self, Project, RemoteFailureMode, Workspace};

use super::dev::DevRequest;
use super::run::{CacheOptions, Parallelism, ProjectSelection, RunRequest};
use super::watch::WatchRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskRunRequest {
    pub(crate) name: String,
    pub(crate) project: Option<String>,
    pub(crate) parallelism: Parallelism,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskRunManyRequest {
    pub(crate) name: String,
    pub(crate) all: bool,
    pub(crate) projects: Vec<String>,
    pub(crate) parallelism: Parallelism,
}

#[derive(Debug, Serialize)]
struct TaskSummary<'a> {
    task: &'a str,
    project: Option<&'a str>,
    status: &'static str,
}

pub(crate) fn list(
    cwd: &Path,
    project_name: Option<&str>,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let mut output = String::new();
    if let Some(project_name) = project_name {
        let project = find_project(&workspace, project_name)?;
        output.push_str(&format!("Project tasks for {}:\n", project.name));
        for task in project.gomo_tasks.values() {
            render_task(&mut output, task);
        }
    } else {
        output.push_str("Workspace tasks:\n");
        for task in workspace
            .tasks
            .values()
            .filter(|task| task.scope == TaskScope::Workspace)
        {
            render_task(&mut output, task);
        }
        let reusable = workspace
            .tasks
            .values()
            .filter(|task| task.scope == TaskScope::Project)
            .collect::<Vec<_>>();
        if !reusable.is_empty() {
            output.push_str("\nReusable project tasks:\n");
            for task in reusable {
                render_task(&mut output, task);
            }
        }
    }
    if output_options.json {
        let tasks = if let Some(project_name) = project_name {
            find_project(&workspace, project_name)?
                .gomo_tasks
                .values()
                .collect::<Vec<_>>()
        } else {
            workspace.tasks.values().collect::<Vec<_>>()
        };
        let json = tasks
            .into_iter()
            .map(|task| {
                serde_json::json!({
                    "name": task.name,
                    "description": task.description,
                    "scope": match task.scope {
                        TaskScope::Workspace => "workspace",
                        TaskScope::Project => "project",
                    },
                    "persistent": task.persistent,
                    "cacheable": task.cache,
                })
            })
            .collect::<Vec<_>>();
        return Ok(CommandOutput::success(format!(
            "{}\n",
            serde_json::to_string_pretty(&json)?
        )));
    }
    Ok(CommandOutput::success(output))
}

pub(crate) fn explain(
    cwd: &Path,
    name: &str,
    project_name: Option<&str>,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let task = select_task(&workspace, name, project_name)?;
    let inputs = named_task_input_files(&workspace, task, project_name)?
        .into_iter()
        .map(|path| {
            path.strip_prefix(&workspace.root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    let missing_required_inputs =
        named_task_missing_inputs(&workspace, task, project_name, &inputs)?;
    let hash = named_cache_entry(&workspace, task, project_name)?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if output_options.json {
        return Ok(CommandOutput::success(format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "identity": task_identity(task, project_name),
                "description": task.description,
                "scope": if task.scope == TaskScope::Workspace { "workspace" } else { "project" },
                "persistent": task.persistent,
                "cacheable": task.cache,
                "hash": hash,
                "depends_on": task.depends_on,
                "inputs": inputs,
                "missing_required_inputs": missing_required_inputs,
                "outputs": task.outputs,
                "env_inputs": task.env_inputs,
                "steps": task
                    .steps
                    .iter()
                    .map(|step| step.action_name())
                    .collect::<Result<Vec<_>>>()?,
            }))?
        )));
    }
    let mut output = format!(
        "Named Task\nIdentity: {}\nScope: {}\nPersistent: {}\nCacheable: {}\nHash: {}\n",
        task_identity(task, project_name),
        if task.scope == TaskScope::Workspace {
            "workspace"
        } else {
            "project"
        },
        task.persistent,
        task.cache,
        hash
    );
    output.push_str("Dependencies:\n");
    render_values(&mut output, &task.depends_on);
    output.push_str("Declared Inputs:\n");
    render_values(
        &mut output,
        &task
            .inputs
            .iter()
            .map(|input| input.glob().to_string())
            .collect::<Vec<_>>(),
    );
    output.push_str("Matched Inputs:\n");
    render_values(&mut output, &inputs);
    output.push_str("Missing required inputs:\n");
    render_values(&mut output, &missing_required_inputs);
    output.push_str("Outputs:\n");
    render_values(&mut output, &task.outputs);
    output.push_str("Environment Inputs:\n");
    render_values(&mut output, &task.env_inputs);
    output.push_str("Steps:\n");
    for (index, step) in task.steps.iter().enumerate() {
        output.push_str(&format!("{}. {}\n", index + 1, step.action_name()?));
    }
    Ok(CommandOutput::success(output))
}

fn named_task_missing_inputs(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    matched_inputs: &[String],
) -> Result<Vec<String>> {
    let project = project_name
        .map(|name| find_project(workspace, name))
        .transpose()?;
    let mut missing = Vec::new();
    for input in &task.inputs {
        if input.optional() {
            continue;
        }
        let pattern = workspace_relative_pattern(workspace, project, input.glob())?;
        let matcher = Glob::new(&pattern)
            .with_context(|| format!("invalid input glob `{pattern}`"))?
            .compile_matcher();
        if !matched_inputs
            .iter()
            .any(|path| matcher.is_match(Path::new(path)))
        {
            missing.push(input.glob().to_string());
        }
    }
    Ok(missing)
}

pub(crate) fn graph(
    cwd: &Path,
    name: &str,
    project_name: Option<&str>,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let task = select_task(&workspace, name, project_name)?;
    let mut edges = BTreeSet::new();
    collect_task_edges(&workspace, task, project_name, &mut edges)?;
    if output_options.json {
        let edges = edges
            .iter()
            .map(|(from, to)| serde_json::json!({ "from": from, "to": to }))
            .collect::<Vec<_>>();
        return Ok(CommandOutput::success(format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "task": task_identity(task, project_name),
                "edges": edges,
            }))?
        )));
    }
    let mut output = format!("Task graph for {}:\n", task_identity(task, project_name));
    if edges.is_empty() {
        output.push_str("  (no task edges)\n");
    } else {
        for (from, to) in edges {
            output.push_str(&format!("  {from} -> {to}\n"));
        }
    }
    Ok(CommandOutput::success(output))
}

fn collect_task_edges(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    edges: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    let from = task_identity(task, project_name);
    for dependency in &task.depends_on {
        let target = select_task(workspace, dependency, project_name)?;
        let to = task_identity(target, project_name);
        if edges.insert((from.clone(), to)) {
            collect_task_edges(workspace, target, project_name, edges)?;
        }
    }
    for step in &task.steps {
        if let Some(action) = &step.task {
            let target_project = action.project.as_deref().or(project_name);
            let target = select_task(workspace, &action.name, target_project)?;
            let to = task_identity(target, target_project);
            if edges.insert((from.clone(), to)) {
                collect_task_edges(workspace, target, target_project, edges)?;
            }
        }
    }
    Ok(())
}

fn task_identity(task: &Task, project_name: Option<&str>) -> String {
    project_name
        .map(|project| format!("{project}:{}", task.name))
        .unwrap_or_else(|| task.name.clone())
}

fn render_values(output: &mut String, values: &[String]) {
    if values.is_empty() {
        output.push_str("- (none)\n");
    } else {
        for value in values {
            output.push_str(&format!("- {value}\n"));
        }
    }
}

fn render_task(output: &mut String, task: &Task) {
    let description = task.description.as_deref().unwrap_or("");
    let cached = if task.cache { "  cached" } else { "" };
    let persistent = if task.persistent { "  persistent" } else { "" };
    output.push_str(&format!(
        "  {:<20} {}{}{}\n",
        task.name, description, cached, persistent
    ));
}

pub(crate) fn run(
    cwd: &Path,
    request: TaskRunRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    if std::env::var_os("GOMO_TASK_ACTIVE").is_some() {
        bail!(
            "a child command attempted to run `gomo task`; use a native task action instead of invoking Gomo recursively"
        );
    }
    let workspace = workspace::discover_from(cwd)?;
    let task = select_task(&workspace, &request.name, request.project.as_deref())?;
    if output_options.json && task.persistent {
        bail!(
            "--json is not supported for persistent task `{}`",
            task.name
        );
    }
    let state = Arc::new(Mutex::new(ExecutionState::default()));
    let output = execute_task(
        &workspace,
        task,
        request.project.as_deref(),
        request.parallelism,
        cache_options,
        output_options.clone(),
        &state,
    )?;
    if output_options.json {
        let summary = TaskSummary {
            task: &request.name,
            project: request.project.as_deref(),
            status: "succeeded",
        };
        return Ok(CommandOutput::success(format!(
            "{}\n",
            serde_json::to_string_pretty(&summary)?
        )));
    }
    Ok(CommandOutput::success(output))
}

pub(crate) fn run_many(
    cwd: &Path,
    request: TaskRunManyRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    if std::env::var_os("GOMO_TASK_ACTIVE").is_some() {
        bail!(
            "a child command attempted to run `gomo task`; use a native task action instead of invoking Gomo recursively"
        );
    }
    let workspace = workspace::discover_from(cwd)?;
    let project_names = if request.all {
        workspace
            .projects
            .iter()
            .filter(|project| project.gomo_tasks.contains_key(&request.name))
            .map(|project| project.name.clone())
            .collect::<Vec<_>>()
    } else {
        request.projects.clone()
    };
    if project_names.is_empty() {
        bail!(
            "no projects explicitly expose project task `{}`",
            request.name
        );
    }
    for project_name in &project_names {
        let project = find_project(&workspace, project_name)?;
        if !project.gomo_tasks.contains_key(&request.name) {
            bail!(
                "project `{project_name}` does not expose task `{}`",
                request.name
            );
        }
    }

    let mut output = String::new();
    let state = Arc::new(Mutex::new(ExecutionState::default()));
    for project_name in project_names {
        let task = select_task(&workspace, &request.name, Some(&project_name))?;
        if output_options.json && task.persistent {
            bail!(
                "--json is not supported for persistent task `{}`",
                task.name
            );
        }
        let task_output = execute_task(
            &workspace,
            task,
            Some(&project_name),
            request.parallelism,
            cache_options,
            output_options.clone(),
            &state,
        )?;
        if output_options.json {
            let summary = TaskSummary {
                task: &request.name,
                project: Some(&project_name),
                status: "succeeded",
            };
            output.push_str(&format!("{}\n", serde_json::to_string_pretty(&summary)?));
        } else {
            output.push_str(&task_output);
        }
    }
    Ok(CommandOutput::success(output))
}

#[derive(Default)]
struct ExecutionState {
    completed: BTreeSet<String>,
    locks: BTreeMap<String, Arc<Mutex<()>>>,
    project_preparation_locks: BTreeMap<String, Arc<Mutex<()>>>,
}

fn select_task<'a>(
    workspace: &'a Workspace,
    name: &str,
    project_name: Option<&str>,
) -> Result<&'a Task> {
    if let Some(project_name) = project_name {
        let project = find_project(workspace, project_name)?;
        return project.gomo_tasks.get(name).ok_or_else(|| {
            unknown_task(
                name,
                &format!("project `{project_name}`"),
                project.gomo_tasks.keys().map(String::as_str),
            )
        });
    }
    match workspace.tasks.get(name) {
        Some(task) if task.scope == TaskScope::Workspace => Ok(task),
        _ => Err(unknown_task(
            name,
            "workspace",
            workspace
                .tasks
                .values()
                .filter(|task| task.scope == TaskScope::Workspace)
                .map(|task| task.name.as_str()),
        )),
    }
}

fn execute_task(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    parallelism: Parallelism,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    state: &Arc<Mutex<ExecutionState>>,
) -> Result<String> {
    if cache_options.require_remote_cache
        && !cache_options.no_remote_cache
        && workspace.remote_cache.is_none()
    {
        bail!("--require-remote-cache was set, but no remote cache is configured");
    }
    let identity = project_name
        .map(|project| format!("{project}:{}", task.name))
        .unwrap_or_else(|| task.name.clone());
    let task_lock = {
        let mut state = state
            .lock()
            .map_err(|_| anyhow!("task completion state was poisoned"))?;
        if state.completed.contains(&identity) {
            return Ok(String::new());
        }
        state
            .locks
            .entry(identity.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = task_lock
        .lock()
        .map_err(|_| anyhow!("task execution lock was poisoned"))?;
    {
        let state = state
            .lock()
            .map_err(|_| anyhow!("task completion state was poisoned"))?;
        if state.completed.contains(&identity) {
            return Ok(String::new());
        }
    }

    let mut output = String::new();
    for dependency in &task.depends_on {
        let dependency_task = select_task(workspace, dependency, project_name)?;
        output.push_str(&execute_task(
            workspace,
            dependency_task,
            project_name,
            parallelism,
            cache_options,
            output_options.clone(),
            state,
        )?);
    }

    let cache_entry = if task.cache && !cache_options.no_cache {
        let entry = named_cache_entry(workspace, task, project_name)?;
        if !cache_options.no_restore
            && let Some(cached) = named_cache_hit(workspace, task, project_name, &entry, false)?
        {
            state
                .lock()
                .map_err(|_| anyhow!("task completion state was poisoned"))?
                .completed
                .insert(identity.clone());
            output.push_str(&format!("✓ {identity} (cached)\n"));
            output.push_str(&cached.stdout);
            output.push_str(&cached.stderr);
            return Ok(output);
        }
        if !cache_options.no_restore && !cache_options.no_remote_cache && task.remote_cache {
            let descriptor = named_task_descriptor(workspace, task, project_name, &entry)?;
            match RemoteCacheClient::from_workspace(workspace).and_then(|client| {
                client
                    .map(|client| client.restore_named(workspace, &descriptor))
                    .transpose()
            }) {
                Ok(Some(Some(hit))) => {
                    let cached = named_cache_hit(
                        workspace,
                        task,
                        project_name,
                        &entry,
                        false,
                    )?
                    .context(
                        "remote named-task entry was imported but could not be restored locally",
                    )?;
                    state
                        .lock()
                        .map_err(|_| anyhow!("task completion state was poisoned"))?
                        .completed
                        .insert(identity.clone());
                    output.push_str(&format!(
                        "✓ {identity} (cached remote scope={} bytes={})\n",
                        hit.scope, hit.bytes_downloaded
                    ));
                    output.push_str(&cached.stdout);
                    output.push_str(&cached.stderr);
                    return Ok(output);
                }
                Ok(Some(None) | None) => {}
                Err(error) if named_remote_failure_is_fatal(workspace, cache_options) => {
                    return Err(error);
                }
                Err(error) => {
                    output.push_str(&format!("[remote cache warning] {error}\n"));
                }
            }
        }
        Some(entry)
    } else {
        None
    };

    // Top-level persistent tasks own one outer session (TUI or CI prefixes).
    // Nested persistent work receives a parent surface and must not open another.
    if task.persistent && output_options.surface.is_none() {
        let supervised = execute_persistent_steps(
            workspace,
            task,
            project_name,
            parallelism,
            cache_options,
            output_options.clone(),
            state,
            &identity,
        )?;
        output.push_str(&supervised);
    } else {
        let step_output_options = if task.persistent {
            output_options.clone().without_tui()
        } else {
            output_options.clone()
        };
        match task.mode {
            TaskMode::Sequential => {
                for step in &task.steps {
                    output.push_str(&execute_step(
                        workspace,
                        task,
                        step,
                        project_name,
                        parallelism,
                        cache_options,
                        step_output_options.clone(),
                        state,
                    )?);
                }
            }
            TaskMode::Parallel => {
                let target_task_stack = target_task_stack();
                let results = thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for step in &task.steps {
                        let step_output_options = step_output_options.clone();
                        let target_task_stack = target_task_stack.clone();
                        handles.push(scope.spawn(|| {
                            with_target_task_stack(target_task_stack, || {
                                execute_step(
                                    workspace,
                                    task,
                                    step,
                                    project_name,
                                    parallelism,
                                    cache_options,
                                    step_output_options,
                                    state,
                                )
                            })
                        }));
                    }
                    handles
                        .into_iter()
                        .map(|handle| {
                            handle
                                .join()
                                .map_err(|_| anyhow!("parallel task worker panicked"))?
                        })
                        .collect::<Result<Vec<_>>>()
                })?;
                output.extend(results);
            }
        }
    }
    state
        .lock()
        .map_err(|_| anyhow!("task completion state was poisoned"))?
        .completed
        .insert(identity);
    if cache_options.should_store()
        && let Some(cache_entry) = cache_entry
    {
        store_named_cache_entry(workspace, task, project_name, &cache_entry, &output)?;
        if !cache_options.no_remote_cache
            && !cache_options.remote_cache_read_only
            && task.remote_cache
        {
            let descriptor = named_task_descriptor(workspace, task, project_name, &cache_entry)?;
            match RemoteCacheClient::from_workspace(workspace).and_then(|client| {
                client
                    .map(|client| client.store_named(workspace, &descriptor))
                    .transpose()
            }) {
                Ok(Some(Some(stored))) => output.push_str(&format!(
                    "[cache stored remote scope={} bytes={} outcome={:?}]\n",
                    stored.scope, stored.bytes_uploaded, stored.outcome
                )),
                Ok(Some(None) | None) => {}
                Err(error) if named_remote_failure_is_fatal(workspace, cache_options) => {
                    return Err(error);
                }
                Err(error) => {
                    output.push_str(&format!("[remote cache warning] upload failed: {error}\n"))
                }
            }
        }
    }
    Ok(output)
}

fn named_remote_failure_is_fatal(workspace: &Workspace, cache_options: CacheOptions) -> bool {
    cache_options.require_remote_cache
        || workspace
            .remote_cache
            .as_ref()
            .is_some_and(|config| config.failure == RemoteFailureMode::Error)
}

fn step_identity(step: &TaskStep, index: usize) -> String {
    if let Some(action) = &step.dev {
        return format!("{}:dev", action.project);
    }
    if let Some(action) = &step.task {
        return match action.project.as_deref() {
            Some(project) => format!("{project}:{}", action.name),
            None => action.name.clone(),
        };
    }
    if let Some(action) = &step.target {
        return match action.project.as_deref() {
            Some(project) => format!("{project}:{}", action.target),
            None => action.target.clone(),
        };
    }
    if let Some(action) = &step.watch {
        return match action.project.as_deref() {
            Some(project) => format!("{project}:watch-{}", action.target),
            None => format!("watch-{}", action.target),
        };
    }
    if let Some(action) = &step.module {
        return format!("{}:{}", action.project, action.module);
    }
    if step.exec.is_some() {
        return format!("exec-{}", index + 1);
    }
    if step.shell.is_some() {
        return format!("shell-{}", index + 1);
    }
    format!("step-{}", index + 1)
}

fn step_command_display(step: &TaskStep) -> String {
    if let Some(action) = &step.dev {
        if action.command.is_empty() {
            return format!("gomo dev --project {}", action.project);
        }
        return format!(
            "gomo dev --project {} -- {}",
            action.project,
            action.command.join(" ")
        );
    }
    if let Some(action) = &step.task {
        return match action.project.as_deref() {
            Some(project) => format!("gomo task run {} --project {project}", action.name),
            None => format!("gomo task run {}", action.name),
        };
    }
    if let Some(action) = &step.shell {
        return action.command.clone();
    }
    if let Some(action) = &step.exec {
        return std::iter::once(action.program.as_str())
            .chain(action.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
    }
    if let Some(action) = &step.module {
        return format!("gleam run -m {}", action.module);
    }
    if let Some(action) = &step.target {
        return format!("gomo {}", action.target);
    }
    if let Some(action) = &step.watch {
        return format!("gomo watch --target {}", action.target);
    }
    "gomo task step".to_string()
}

#[allow(clippy::too_many_arguments)]
fn execute_persistent_steps(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    parallelism: Parallelism,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    state: &Arc<Mutex<ExecutionState>>,
    identity: &str,
) -> Result<String> {
    let step_ids = task
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| step_identity(step, index))
        .collect::<Vec<_>>();
    let command_displays = task
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step_identity(step, index), step_command_display(step)))
        .collect::<BTreeMap<_, _>>();

    let use_tui = output_options.tui && !output_options.ci && !output_options.json;
    let cancellation = CancellationToken::new();
    let (event_tx, event_rx, _) = RenderSurface::channel(cancellation.clone(), output_options.ci);
    let event_tx = Arc::new(event_tx);

    let mut terminal = use_tui.then(|| {
        let mut terminal = RunManyTerminal::new_persistent(
            &step_ids,
            identity,
            command_displays.clone(),
            parallelism.resolve(workspace.default_parallelism),
        );
        let _ = terminal.start();
        for step_id in &step_ids {
            let _ = terminal.task_started(step_id);
        }
        terminal
    });

    let (done_tx, done_rx) = std::sync::mpsc::channel::<(String, Result<String>)>();
    let done_tx = Arc::new(done_tx);
    let step_count = task.steps.len();
    let target_task_stack = target_task_stack();

    thread::scope(|scope| {
        match task.mode {
            TaskMode::Sequential => {
                let event_tx = Arc::clone(&event_tx);
                let done_tx = Arc::clone(&done_tx);
                let output_options = output_options.clone();
                let cancellation = cancellation.clone();
                let target_task_stack = target_task_stack.clone();
                scope.spawn(move || {
                    with_target_task_stack(target_task_stack, || {
                        for (index, step) in task.steps.iter().enumerate() {
                            if cancellation.is_cancelled() {
                                break;
                            }
                            let step_id = step_identity(step, index);
                            let surface = RenderSurface::new(
                                step_id.clone(),
                                (*event_tx).clone(),
                                cancellation.clone(),
                                output_options.ci,
                            )
                            .with_color(use_tui);
                            surface.mark_started();
                            let step_options = output_options.clone().with_surface(surface.clone());
                            let result = execute_step(
                                workspace,
                                task,
                                step,
                                project_name,
                                parallelism,
                                cache_options,
                                step_options,
                                state,
                            );
                            surface.mark_finished(result.is_ok());
                            if done_tx.send((step_id, result)).is_err() {
                                break;
                            }
                        }
                    });
                });
            }
            TaskMode::Parallel => {
                for (index, step) in task.steps.iter().enumerate() {
                    let step_id = step_identity(step, index);
                    let surface = RenderSurface::new(
                        step_id.clone(),
                        (*event_tx).clone(),
                        cancellation.clone(),
                        output_options.ci,
                    )
                    .with_color(use_tui);
                    let done_tx = Arc::clone(&done_tx);
                    let output_options = output_options.clone();
                    let target_task_stack = target_task_stack.clone();
                    scope.spawn(move || {
                        with_target_task_stack(target_task_stack, || {
                            surface.mark_started();
                            let step_options = output_options.with_surface(surface.clone());
                            let result = execute_step(
                                workspace,
                                task,
                                step,
                                project_name,
                                parallelism,
                                cache_options,
                                step_options,
                                state,
                            );
                            surface.mark_finished(result.is_ok());
                            let _ = done_tx.send((step_id, result));
                        });
                    });
                }
            }
        }
        drop(done_tx);
        drop(event_tx);

        let mut remaining = step_count;
        let mut outputs = BTreeMap::<String, String>::new();
        let mut first_error: Option<anyhow::Error> = None;

        while remaining > 0 {
            // Each worker sends its surface status before its completion. Drain
            // completions first so the final status is rendered before exit.
            while let Ok((step_id, result)) = done_rx.try_recv() {
                remaining = remaining.saturating_sub(1);
                match result {
                    Ok(text) => {
                        outputs.insert(step_id, text);
                    }
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        cancellation.cancel();
                    }
                }
            }

            let mut logs_dirty = false;
            while let Ok(event) = event_rx.try_recv() {
                if let Some(terminal) = terminal.as_mut() {
                    match event {
                        SurfaceEvent::Log { step, chunk } => {
                            terminal.append_log_silent(&step, &chunk);
                            logs_dirty = true;
                        }
                        SurfaceEvent::StepStarted { step } => {
                            let _ = terminal.task_started(&step);
                        }
                        SurfaceEvent::StepFinished { step, ok } => {
                            use crate::commands::run::{TaskOutcome, TaskStatus};
                            let outcome = TaskOutcome {
                                project: step,
                                status: if ok {
                                    TaskStatus::Succeeded
                                } else {
                                    TaskStatus::Failed(1)
                                },
                                cache_status: None,
                                cache_source: None,
                                remote_scope: None,
                                bytes_downloaded: 0,
                                bytes_uploaded: 0,
                                upload_result: None,
                            };
                            let _ = terminal.task_completed(&outcome, "");
                        }
                    }
                } else if let SurfaceEvent::Log { step, chunk } = event {
                    // Non-TUI path: CI prefixes are printed in append_log; plain
                    // mode still needs the demuxed chunk on stdout.
                    if !output_options.ci {
                        print!("[{step}] {chunk}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                }
            }
            if logs_dirty && let Some(terminal) = terminal.as_mut() {
                let _ = terminal.tick();
            }

            if let Some(terminal) = terminal.as_mut() {
                if terminal.poll_quit().unwrap_or(false) {
                    cancellation.cancel();
                    first_error =
                        first_error.or_else(|| Some(anyhow!("persistent task cancelled")));
                }
                let _ = terminal.tick();
            }

            if cancellation.is_cancelled() && remaining == 0 {
                break;
            }
            if remaining == 0 {
                break;
            }
            // When cancelled, keep draining until workers notice and exit.
            thread::sleep(Duration::from_millis(30));
        }

        if let Some(terminal) = terminal.as_mut() {
            let _ = terminal.restore_now();
        }

        if let Some(error) = first_error {
            // User quit should be a clean exit code 0 path for task run.
            if error.to_string().contains("cancelled") {
                return Ok(String::new());
            }
            return Err(error);
        }

        Ok(outputs.into_values().collect::<Vec<_>>().join(""))
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_step(
    workspace: &Workspace,
    owner: &Task,
    step: &TaskStep,
    current_project: Option<&str>,
    parallelism: Parallelism,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    state: &Arc<Mutex<ExecutionState>>,
) -> Result<String> {
    // Nested native work under a surface must never open its own TUI.
    let nested_options = if output_options.surface.is_some() {
        output_options.clone().without_tui()
    } else {
        output_options.clone()
    };
    let surface = nested_options.surface.clone();

    if let Some(action) = &step.task {
        let project = action.project.as_deref().or(current_project);
        let task = select_task(workspace, &action.name, project)?;
        return execute_task(
            workspace,
            task,
            project,
            parallelism,
            cache_options,
            nested_options,
            state,
        );
    }
    if let Some(action) = &step.target {
        let project = required_project(action.project.as_deref().or(current_project), owner)?;
        let target = parse_target(&action.target)?;
        return native_output(
            super::run::run(
                &workspace.root,
                RunRequest {
                    target,
                    command_options: CommandOptions::default(),
                    selection: ProjectSelection::Project(project.to_string()),
                    with_deps: action.with_deps,
                    parallelism,
                },
                cache_options,
                nested_options,
            )?,
            surface.as_ref(),
        );
    }
    if let Some(action) = &step.dev {
        return native_output(
            super::dev::run(
                &workspace.root,
                DevRequest {
                    project: action.project.clone(),
                    command: action.command.clone(),
                    debounce: Duration::from_millis(300),
                    reload: None,
                    parallelism,
                },
                cache_options,
                nested_options,
            )?,
            surface.as_ref(),
        );
    }
    if let Some(action) = &step.watch {
        let target = parse_target(&action.target)?;
        return native_output(
            super::watch::run(
                &workspace.root,
                WatchRequest {
                    target: Some(target),
                    selection: action
                        .project
                        .as_deref()
                        .or(current_project)
                        .map(|project| ProjectSelection::Project(project.to_string()))
                        .unwrap_or(ProjectSelection::All),
                    with_deps: false,
                    initial_run: action.initial_run,
                    debounce: Duration::from_millis(300),
                    callback: Vec::new(),
                    parallelism,
                },
                cache_options,
                nested_options,
            )?,
            surface.as_ref(),
        );
    }
    if let Some(action) = &step.module {
        let project = find_project(workspace, &action.project)?;
        let preparation_lock = {
            let mut state = state
                .lock()
                .map_err(|_| anyhow!("task completion state was poisoned"))?;
            state
                .project_preparation_locks
                .entry(project.name.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let preparation_guard = preparation_lock
            .lock()
            .map_err(|_| anyhow!("project preparation lock was poisoned"))?;
        crate::dependency_vendor::prepare_projects(workspace, std::slice::from_ref(&project.name))?;
        drop(preparation_guard);
        let mut args = vec!["run".to_string(), "-m".to_string(), action.module.clone()];
        if !action.args.is_empty() {
            args.push("--".to_string());
            args.extend(action.args.iter().cloned());
        }
        return run_process(
            workspace,
            owner,
            &project.root,
            Some(&project.root),
            "gleam",
            &args,
            &Default::default(),
            true,
            surface.as_ref(),
        );
    }
    if let Some(action) = &step.exec {
        let cwd = action_cwd(workspace, current_project, action.cwd.as_deref())?;
        let project_root = current_project
            .map(|name| find_project(workspace, name).map(|project| project.root.as_path()))
            .transpose()?;
        return run_exec(
            workspace,
            owner,
            &cwd,
            project_root,
            action,
            surface.as_ref(),
        );
    }
    if let Some(action) = &step.shell {
        let cwd = action_cwd(workspace, current_project, action.cwd.as_deref())?;
        let project_root = current_project
            .map(|name| find_project(workspace, name).map(|project| project.root.as_path()))
            .transpose()?;
        return run_shell(
            workspace,
            owner,
            &cwd,
            project_root,
            action,
            surface.as_ref(),
        );
    }
    unreachable!("task steps are validated during discovery")
}

fn native_output(output: CommandOutput, surface: Option<&RenderSurface>) -> Result<String> {
    if let Some(surface) = surface
        && !output.stdout.is_empty()
    {
        surface.append_log(&output.stdout);
    }
    if output.is_success() {
        Ok(output.stdout)
    } else {
        bail!(
            "native task action failed with exit code {}",
            output.exit_code
        )
    }
}

fn run_exec(
    workspace: &Workspace,
    owner: &Task,
    cwd: &Path,
    project_root: Option<&Path>,
    action: &ExecAction,
    surface: Option<&RenderSurface>,
) -> Result<String> {
    if Path::new(&action.program)
        .file_name()
        .is_some_and(|program| program == "gomo")
    {
        bail!(
            "task `{}` invokes Gomo through an exec action; use a native task action instead",
            owner.name
        );
    }
    run_process(
        workspace,
        owner,
        cwd,
        project_root,
        &action.program,
        &action.args,
        &action.env,
        true,
        surface,
    )
}

fn run_shell(
    workspace: &Workspace,
    owner: &Task,
    cwd: &Path,
    project_root: Option<&Path>,
    action: &ShellAction,
    surface: Option<&RenderSurface>,
) -> Result<String> {
    run_process(
        workspace,
        owner,
        cwd,
        project_root,
        "sh",
        &["-c".to_string(), action.command.clone()],
        &action.env,
        false,
        surface,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_process(
    workspace: &Workspace,
    owner: &Task,
    cwd: &Path,
    project_root: Option<&Path>,
    program: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    interpolate_args: bool,
    surface: Option<&RenderSurface>,
) -> Result<String> {
    let mut command = Command::new(if interpolate_args {
        interpolate(program, workspace, project_root)
    } else {
        program.to_string()
    });
    command
        .args(args.iter().map(|arg| {
            if interpolate_args {
                interpolate(arg, workspace, project_root)
            } else {
                arg.clone()
            }
        }))
        .current_dir(cwd)
        .env("GOMO_TASK_ACTIVE", "1")
        .env(
            "GOMO_WORKSPACE_ROOT",
            workspace.root.to_string_lossy().as_ref(),
        );
    if let Some(project_root) = project_root {
        command.env("GOMO_PROJECT_ROOT", project_root.to_string_lossy().as_ref());
    }
    for (name, value) in env {
        command.env(name, interpolate(value, workspace, project_root));
    }
    if owner.persistent {
        if let Some(surface) = surface {
            return surface::run_piped_persistent(command, surface, || {
                format!(
                    "persistent action in task `{}` exited unexpectedly with status 0",
                    owner.name
                )
            });
        }
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to run `{program}` in {}", cwd.display()))?;
        if status.success() {
            bail!(
                "persistent action in task `{}` exited unexpectedly with status 0",
                owner.name
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.signal().is_some() {
                return Ok(String::new());
            }
        }
        let code = status.code().unwrap_or(1);
        bail!("task `{}` failed with exit code {code}", owner.name);
    }
    if let Some(surface) = surface {
        surface::configure_captured_output(&mut command, surface.color_enabled());
    }
    let output = command
        .output()
        .with_context(|| format!("failed to run `{program}` in {}", cwd.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if let Some(surface) = surface {
        surface.append_log(&text);
    }
    if !output.status.success() {
        bail!(
            "task `{}` failed running `{program}` with exit code {}\n{text}",
            owner.name,
            output.status.code().unwrap_or(1)
        );
    }
    Ok(text)
}

fn action_cwd(
    workspace: &Workspace,
    current_project: Option<&str>,
    configured: Option<&str>,
) -> Result<PathBuf> {
    let project_root = current_project
        .map(|name| find_project(workspace, name).map(|project| project.root.as_path()))
        .transpose()?;
    let configured = configured.unwrap_or(if project_root.is_some() {
        "{project_root}"
    } else {
        "{workspace_root}"
    });
    let interpolated = interpolate(configured, workspace, project_root);
    let path = PathBuf::from(interpolated);
    let path = if path.is_absolute() {
        path
    } else {
        workspace.root.join(path)
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("task working directory {} does not exist", path.display()))?;
    if !path.starts_with(&workspace.root) {
        bail!(
            "task working directory {} resolves outside the workspace",
            path.display()
        );
    }
    Ok(path)
}

fn interpolate(value: &str, workspace: &Workspace, project_root: Option<&Path>) -> String {
    let value = value.replace(
        "{workspace_root}",
        workspace.root.to_string_lossy().as_ref(),
    );
    match project_root {
        Some(project_root) => {
            value.replace("{project_root}", project_root.to_string_lossy().as_ref())
        }
        None => value,
    }
}

fn required_project<'a>(project: Option<&'a str>, owner: &Task) -> Result<&'a str> {
    project.ok_or_else(|| {
        anyhow!(
            "target action in workspace task `{}` must specify a project",
            owner.name
        )
    })
}

fn find_project<'a>(workspace: &'a Workspace, name: &str) -> Result<&'a Project> {
    workspace
        .projects
        .iter()
        .find(|project| project.name == name)
        .ok_or_else(|| anyhow!("unknown project `{name}`"))
}

fn parse_target(value: &str) -> Result<Target> {
    match value {
        "build" => Ok(Target::Build),
        "clean" => Ok(Target::Clean),
        "format" => Ok(Target::Format),
        "test" => Ok(Target::Test),
        _ => bail!("unknown native target `{value}`; expected build, clean, format, or test"),
    }
}

fn unknown_task<'a>(
    name: &str,
    scope: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> anyhow::Error {
    let suggestion = candidates
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .min()
        .filter(|(distance, _)| *distance <= 3)
        .map(|(_, candidate)| format!("; did you mean `{candidate}`?"))
        .unwrap_or_default();
    anyhow!("unknown task `{name}` in {scope} scope{suggestion}")
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.bytes().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_byte) in right.bytes().enumerate() {
            current.push(
                (previous[right_index + 1] + 1)
                    .min(current[right_index] + 1)
                    .min(previous[right_index] + usize::from(left_byte != right_byte)),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

fn named_cache_entry(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
) -> Result<PathBuf> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gomo-named-task-v1\0");
    hasher.update(format!("{task:?}").as_bytes());
    if let Some(project_name) = project_name {
        hasher.update(project_name.as_bytes());
    }
    for env_name in &task.env_inputs {
        hasher.update(env_name.as_bytes());
        hasher.update(
            std::env::var_os(env_name)
                .unwrap_or_default()
                .to_string_lossy()
                .as_bytes(),
        );
    }
    for tool in &task.tools {
        let fingerprint = toolchain_fingerprint(workspace, tool)?;
        hasher.update(tool.program.as_bytes());
        for argument in &tool.args {
            hasher.update(argument.as_bytes());
        }
        hasher.update(fingerprint.as_bytes());
    }
    for dependency in &task.depends_on {
        let dependency_task = select_task(workspace, dependency, project_name)?;
        match task
            .dependency_cache_strategies
            .get(dependency)
            .copied()
            .unwrap_or_default()
        {
            DependencyCacheStrategy::Hash => {
                let dependency_entry = named_cache_entry(workspace, dependency_task, project_name)?;
                hasher.update(dependency_entry.to_string_lossy().as_bytes());
            }
            DependencyCacheStrategy::Outputs => {
                hasher.update(
                    named_output_digest(workspace, dependency_task, project_name)?.as_bytes(),
                );
            }
            DependencyCacheStrategy::Ignored => {}
        }
    }
    for input in named_task_input_files(workspace, task, project_name)? {
        let relative = input
            .strip_prefix(&workspace.root)
            .unwrap_or(input.as_path());
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(
            &fs::read(&input).with_context(|| format!("failed to hash {}", input.display()))?,
        );
    }
    for step in &task.steps {
        if let Some(action) = &step.module {
            let project = find_project(workspace, &action.project)?;
            for entry in WalkDir::new(&project.root)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !matches!(entry.file_name().to_str(), Some("build" | ".git" | ".gomo"))
                        || entry.depth() > 1
                })
            {
                let entry = entry?;
                if entry.file_type().is_file() {
                    hasher.update(
                        entry
                            .path()
                            .strip_prefix(&workspace.root)
                            .unwrap_or(entry.path())
                            .to_string_lossy()
                            .as_bytes(),
                    );
                    hasher.update(&fs::read(entry.path())?);
                }
            }
        }
    }
    let identity = project_name
        .map(|project| format!("{project}:{}", task.name))
        .unwrap_or_else(|| task.name.clone());
    let hash = hasher.finalize().to_hex().to_string();
    let command = format!("{:?}", task.steps);
    let descriptor = cache::NamedTaskCacheDescriptor {
        hash_manifest: named_task_hash_manifest(
            workspace,
            task,
            project_name,
            &identity,
            &command,
            &hash,
        )?,
        identity,
        task_name: task.name.clone(),
        command,
        hash,
        declared_outputs: task.outputs.clone(),
    };
    Ok(cache::named_task_cache_entry_dir(workspace, &descriptor))
}

fn named_output_digest(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut outputs = named_output_paths(workspace, task, project_name)?;
    outputs.sort();
    for output in outputs {
        if !output.exists() {
            bail!(
                "dependency task `{}` is missing declared output {}",
                task.name,
                output.display()
            );
        }
        for entry in WalkDir::new(&output)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&workspace.root)?;
            hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_file() {
                hasher.update(&metadata.len().to_le_bytes());
                let mut file = File::open(entry.path())?;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = file.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
            } else if metadata.file_type().is_symlink() {
                hasher.update(fs::read_link(entry.path())?.to_string_lossy().as_bytes());
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn toolchain_fingerprint(
    workspace: &Workspace,
    tool: &crate::task::ToolchainProbe,
) -> Result<String> {
    const TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_OUTPUT: u64 = 64 * 1024;
    let mut stdout = tempfile::tempfile().context("failed to create tool probe stdout spool")?;
    let mut stderr = tempfile::tempfile().context("failed to create tool probe stderr spool")?;
    let mut command = Command::new(&tool.program);
    command
        .args(&tool.args)
        .current_dir(&workspace.root)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if tool.optional && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("optional-tool-missing".to_string());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to run toolchain probe `{}`", tool.program));
        }
    };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("failed to poll toolchain probe")? {
            break status;
        }
        if started.elapsed() >= TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "toolchain probe `{}` timed out after 5 seconds",
                tool.program
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    stdout
        .seek(SeekFrom::Start(0))
        .context("failed to rewind toolchain probe stdout")?;
    stderr
        .seek(SeekFrom::Start(0))
        .context("failed to rewind toolchain probe stderr")?;
    let mut output = Vec::new();
    stdout.take(MAX_OUTPUT + 1).read_to_end(&mut output)?;
    stderr.take(MAX_OUTPUT + 1).read_to_end(&mut output)?;
    if output.len() as u64 > MAX_OUTPUT {
        bail!(
            "toolchain probe `{}` exceeded the 65536-byte output limit",
            tool.program
        );
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.program.as_bytes());
    for argument in &tool.args {
        hasher.update(argument.as_bytes());
    }
    hasher.update(status.code().unwrap_or(-1).to_string().as_bytes());
    hasher.update(&output);
    Ok(hasher.finalize().to_hex().to_string())
}

fn named_task_input_files(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let project = project_name
        .map(|name| find_project(workspace, name))
        .transpose()?;
    let mut patterns = task.inputs.clone();
    // Project tasks with no declared inputs still hash local `dev/**` so package
    // scripts and fixtures invalidate the cache, matching default target inputs.
    if project.is_some() && patterns.is_empty() {
        patterns.push(crate::task::TaskInput::Glob("dev/**".to_string()));
    }
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let mut resolved_patterns = Vec::with_capacity(patterns.len());
    let mut builder = GlobSetBuilder::new();
    for input in patterns {
        let pattern = workspace_relative_pattern(workspace, project, input.glob())?;
        builder.add(
            Glob::new(&pattern).with_context(|| {
                format!("invalid input glob `{pattern}` for task `{}`", task.name)
            })?,
        );
        resolved_patterns.push(pattern);
    }
    let globs = builder.build()?;
    // Project tasks may declare `{workspace_root}/...` inputs outside the package
    // tree. Walk the whole workspace when any resolved pattern leaves the project
    // root; otherwise stay inside the project for cheaper hashing.
    let walk_root = {
        let project_prefix = project.map(|project| {
            project
                .root_relative_path
                .to_string_lossy()
                .replace('\\', "/")
        });
        let needs_workspace_walk = project_prefix.as_ref().is_none_or(|prefix| {
            resolved_patterns.iter().any(|pattern| {
                pattern != prefix
                    && !pattern.starts_with(&format!("{prefix}/"))
                    && !pattern.starts_with(&format!("{prefix}\\"))
            })
        });
        if needs_workspace_walk {
            workspace.root.as_path()
        } else {
            project
                .map(|project| project.root.as_path())
                .unwrap_or(workspace.root.as_path())
        }
    };
    let mut files = WalkDir::new(walk_root)
        .into_iter()
        .filter_entry(|entry| {
            // Only prune ignored metadata directories at the walk root, not nested
            // paths such as `src/build` that may match declared input globs.
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".git"
                            | ".jj"
                            | ".gomo"
                            | ".devenv"
                            | "build"
                            | "target"
                            | "node_modules"
                            | "vendor"
                    )
                )
                || entry.depth() > 1
        })
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .strip_prefix(&workspace.root)
                .is_ok_and(|relative| globs.is_match(relative))
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn workspace_relative_pattern(
    workspace: &Workspace,
    project: Option<&Project>,
    pattern: &str,
) -> Result<String> {
    if pattern.contains("{project_root}") && project.is_none() {
        bail!("workspace task path `{pattern}` uses {{project_root}}");
    }
    let interpolated = interpolate(
        pattern,
        workspace,
        project.map(|project| project.root.as_path()),
    );
    let path = PathBuf::from(&interpolated);
    let relative = if path.is_absolute() {
        path.strip_prefix(&workspace.root)
            .with_context(|| format!("task path `{pattern}` resolves outside the workspace"))?
            .to_path_buf()
    } else if project.is_some()
        && !pattern.contains("{workspace_root}")
        && !pattern.contains("{project_root}")
    {
        project
            .expect("project exists")
            .root_relative_path
            .join(path)
    } else {
        path
    };
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn named_output_paths(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let project = project_name
        .map(|name| find_project(workspace, name))
        .transpose()?;
    task.outputs
        .iter()
        .map(|output| {
            let relative = workspace_relative_pattern(workspace, project, output)?;
            let path = workspace.root.join(relative);
            if !path.starts_with(&workspace.root) {
                bail!("task output `{output}` resolves outside the workspace");
            }
            Ok(path)
        })
        .collect()
}

fn named_cache_hit(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    entry: &Path,
    no_restore: bool,
) -> Result<Option<cache::CachedTaskExecution>> {
    let descriptor = named_task_descriptor(workspace, task, project_name, entry)?;
    if !cache::validate_named_task_entry(entry, &descriptor)? {
        return Ok(None);
    }
    let outputs = named_output_paths(workspace, task, project_name)?;
    if outputs.is_empty() {
        return cache::read_named_task_output(entry).map(Some);
    }
    let archive = entry.join("outputs.tar.zst");
    if no_restore {
        return if outputs.iter().all(|output| output.exists()) {
            cache::read_named_task_output(entry).map(Some)
        } else {
            Ok(None)
        };
    }
    if !archive.is_file() {
        return Ok(None);
    }
    restore_named_outputs(workspace, &outputs, &archive)?;
    cache::read_named_task_output(entry).map(Some)
}

fn store_named_cache_entry(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    entry: &Path,
    output: &str,
) -> Result<()> {
    let descriptor = named_task_descriptor(workspace, task, project_name, entry)?;
    if cache::validate_named_task_entry(entry, &descriptor)? {
        return Ok(());
    }
    let parent = entry.parent().context("named cache entry has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", descriptor.hash, unique_suffix()));
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    fs::create_dir(&temp)?;
    let outputs = named_output_paths(workspace, task, project_name)?;
    let mut output_archive = None;
    if !outputs.is_empty() {
        for output in &outputs {
            if !output.exists() {
                bail!(
                    "cached task `{}` did not create declared output {}",
                    task.name,
                    output.display()
                );
            }
        }
        let archive_path = temp.join("outputs.tar.zst");
        cache::write_workspace_outputs_archive(&archive_path, workspace, &outputs)?;
        output_archive = Some(archive_path);
    }
    cache::write_named_task_metadata(&temp, &descriptor, output, output_archive.as_deref())?;
    if entry.exists() {
        fs::remove_dir_all(entry)?;
    }
    fs::rename(&temp, entry)?;
    Ok(())
}

fn named_task_descriptor(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    entry: &Path,
) -> Result<cache::NamedTaskCacheDescriptor> {
    let hash = entry
        .file_name()
        .context("named cache entry has no hash")?
        .to_string_lossy()
        .to_string();
    let identity = task_identity(task, project_name);
    let command = format!("{:?}", task.steps);
    Ok(cache::NamedTaskCacheDescriptor {
        hash_manifest: named_task_hash_manifest(
            workspace,
            task,
            project_name,
            &identity,
            &command,
            &hash,
        )?,
        identity,
        task_name: task.name.clone(),
        command,
        hash,
        declared_outputs: task.outputs.clone(),
    })
}

fn named_task_hash_manifest(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    identity: &str,
    command: &str,
    hash: &str,
) -> Result<cache::HashManifest> {
    let input_paths = named_task_input_files(workspace, task, project_name)?;
    let matched_inputs = input_paths
        .iter()
        .map(|path| {
            path.strip_prefix(&workspace.root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    let inputs = input_paths
        .iter()
        .zip(&matched_inputs)
        .map(|(path, relative)| {
            let contents =
                fs::read(path).with_context(|| format!("failed to hash {}", path.display()))?;
            Ok(cache::HashManifestInput {
                path: relative.clone(),
                blake3: blake3::hash(&contents).to_hex().to_string(),
                byte_len: contents.len() as u64,
                workspace: project_name.is_none(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut dependencies = Vec::new();
    for dependency in &task.depends_on {
        let dependency_task = select_task(workspace, dependency, project_name)?;
        let (strategy, digest) = match task
            .dependency_cache_strategies
            .get(dependency)
            .copied()
            .unwrap_or_default()
        {
            DependencyCacheStrategy::Hash => {
                let entry = named_cache_entry(workspace, dependency_task, project_name)?;
                (
                    "hash",
                    entry
                        .file_name()
                        .context("dependency cache entry has no hash")?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            DependencyCacheStrategy::Outputs => (
                "outputs",
                named_output_digest(workspace, dependency_task, project_name)?,
            ),
            DependencyCacheStrategy::Ignored => ("ignored", String::new()),
        };
        dependencies.push(cache::HashManifestDependency {
            identity: task_identity(dependency_task, project_name),
            strategy: strategy.to_string(),
            digest,
        });
    }
    let toolchains = task
        .tools
        .iter()
        .map(|tool| {
            Ok(cache::HashManifestToolchain {
                program: tool.program.clone(),
                arguments: tool.args.clone(),
                fingerprint: blake3::hash(toolchain_fingerprint(workspace, tool)?.as_bytes())
                    .to_hex()
                    .to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let environment = task
        .env_inputs
        .iter()
        .map(|name| cache::HashManifestEnvironment {
            name: name.clone(),
            value_blake3: blake3::hash(
                std::env::var_os(name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_bytes(),
            )
            .to_hex()
            .to_string(),
        })
        .collect();
    Ok(cache::HashManifest {
        schema_version: cache::CACHE_SCHEMA_VERSION.to_string(),
        task_hash: hash.to_string(),
        task_identity: identity.to_string(),
        command: command.to_string(),
        arguments: Vec::new(),
        declared_outputs: task.outputs.clone(),
        inputs,
        dependencies,
        toolchains,
        environment,
        gomo_version: env!("CARGO_PKG_VERSION").to_string(),
        operating_system: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        missing_required_inputs: named_task_missing_inputs(
            workspace,
            task,
            project_name,
            &matched_inputs,
        )?,
    })
}

fn restore_named_outputs(workspace: &Workspace, outputs: &[PathBuf], archive: &Path) -> Result<()> {
    let allowed = outputs
        .iter()
        .map(|output| output.strip_prefix(&workspace.root).map(Path::to_path_buf))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let temp_dir = archive.parent().map_or_else(
        || {
            workspace
                .cache_dir
                .join(format!(".named-restore-{}", unique_suffix()))
        },
        |parent| parent.join(format!(".restore-{}", unique_suffix())),
    );
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir).with_context(|| {
        format!(
            "failed to create named-task restore temp directory {}",
            temp_dir.display()
        )
    })?;

    let unpack_result = (|| -> Result<()> {
        let archive_file = File::open(archive)?;
        let decoder = zstd::stream::read::Decoder::new(archive_file)?;
        let mut archive_reader = tar::Archive::new(decoder);
        let mut symlinks = Vec::new();
        for entry in archive_reader.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
                || !allowed.iter().any(|output| path.starts_with(output))
            {
                bail!(
                    "cached named-task archive contains unsafe path {}",
                    path.display()
                );
            }
            let entry_type = entry.header().entry_type();
            if entry_type.is_hard_link() {
                bail!(
                    "cached named-task archive contains unsupported link at {}",
                    path.display()
                );
            }
            if entry_type.is_symlink() {
                let link_target = entry
                    .link_name()
                    .context("failed to read cached named-task symlink target")?
                    .context("cached named-task symlink is missing its target")?
                    .into_owned();
                let resolved_target =
                    resolve_named_output_symlink_target(&path, &link_target, &allowed)?;
                symlinks.push((path, link_target, resolved_target));
                continue;
            }
            let destination = temp_dir.join(&path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.unpack(&destination).with_context(|| {
                format!(
                    "failed to unpack cached named-task entry {} to {}",
                    path.display(),
                    destination.display()
                )
            })?;
        }
        for (path, link_target, resolved_target) in symlinks {
            if !temp_dir.join(&resolved_target).exists() {
                bail!(
                    "cached named-task symlink {} points to missing target {}",
                    path.display(),
                    resolved_target.display()
                );
            }
            let destination = temp_dir.join(&path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            create_named_output_symlink(
                &link_target,
                &destination,
                &temp_dir.join(&resolved_target),
            )?;
        }
        Ok(())
    })();

    if let Err(error) = unpack_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }

    let install_result = (|| -> Result<()> {
        for relative in &allowed {
            let restored = temp_dir.join(relative);
            if !restored.exists() {
                bail!(
                    "cached named-task archive is missing declared output {}",
                    relative.display()
                );
            }
            let destination = workspace.root.join(relative);
            if destination.is_dir() {
                fs::remove_dir_all(&destination)?;
            } else if destination.exists() {
                fs::remove_file(&destination)?;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&restored, &destination).with_context(|| {
                format!(
                    "failed to restore named-task output from {} to {}",
                    restored.display(),
                    destination.display()
                )
            })?;
        }
        Ok(())
    })();

    let _ = fs::remove_dir_all(&temp_dir);
    install_result
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn resolve_named_output_symlink_target(
    symlink_path: &Path,
    link_target: &Path,
    allowed: &[PathBuf],
) -> Result<PathBuf> {
    if link_target.is_absolute() {
        bail!(
            "cached named-task symlink {} has absolute target {}",
            symlink_path.display(),
            link_target.display()
        );
    }
    let mut resolved = symlink_path.parent().unwrap_or(Path::new("")).to_path_buf();
    for component in link_target.components() {
        match component {
            Component::Normal(value) => resolved.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    bail!(
                        "cached named-task symlink {} leaves declared outputs",
                        symlink_path.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "cached named-task symlink {} has invalid target {}",
                    symlink_path.display(),
                    link_target.display()
                );
            }
        }
    }
    if !allowed.iter().any(|output| resolved.starts_with(output)) {
        bail!(
            "cached named-task symlink {} leaves declared outputs",
            symlink_path.display()
        );
    }
    Ok(resolved)
}

#[cfg(unix)]
fn create_named_output_symlink(
    link_target: &Path,
    destination: &Path,
    _resolved_target: &Path,
) -> Result<()> {
    std::os::unix::fs::symlink(link_target, destination).with_context(|| {
        format!(
            "failed to create cached named-task symlink {} -> {}",
            destination.display(),
            link_target.display()
        )
    })
}

#[cfg(windows)]
fn create_named_output_symlink(
    link_target: &Path,
    destination: &Path,
    resolved_target: &Path,
) -> Result<()> {
    let result = if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(link_target, destination)
    } else {
        std::os::windows::fs::symlink_file(link_target, destination)
    };
    result.with_context(|| {
        format!(
            "failed to create cached named-task symlink {} -> {}",
            destination.display(),
            link_target.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    fn workspace_with_tasks() -> TestWorkspace {
        let workspace = TestWorkspace::new("gomo-named-task");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.hello]
description = "Say hello"
steps = [{ exec = { program = "printf", args = ["hello\n"] } }]

[tasks.project-hello]
scope = "project"
description = "Say hello from a project"
        steps = [{ shell = { command = "printf '%s\\n' \"$GOMO_PROJECT_ROOT\"" } }]

[tasks.aggregate]
depends_on = ["hello"]
steps = [{ exec = { program = "printf", args = ["done\n"] } }]
"#,
        );
        workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.tasks.project-hello]
extends = "project-hello"
"#,
        );
        workspace
    }

    #[test]
    fn lists_workspace_and_resolved_project_tasks() {
        let workspace = workspace_with_tasks();

        let workspace_output = list(workspace.path(), None, OutputOptions::default())
            .expect("workspace tasks should list")
            .stdout;
        assert!(workspace_output.contains("hello"));
        assert!(workspace_output.contains("Reusable project tasks:"));

        let project_output = list(workspace.path(), Some("demo"), OutputOptions::default())
            .expect("project tasks should list")
            .stdout;
        assert!(project_output.contains("Project tasks for demo:"));
        assert!(project_output.contains("project-hello"));
    }

    #[test]
    fn runs_dependencies_before_task_steps() {
        let workspace = workspace_with_tasks();

        let output = run(
            workspace.path(),
            TaskRunRequest {
                name: "aggregate".to_string(),
                project: None,
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect("task should run");

        assert_eq!(output.stdout, "hello\ndone\n");
    }

    #[test]
    fn direct_project_task_requires_project_selection() {
        let workspace = workspace_with_tasks();

        let error = run(
            workspace.path(),
            TaskRunRequest {
                name: "project-hello".to_string(),
                project: None,
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect_err("project task should not be selected implicitly");

        assert!(
            error
                .to_string()
                .contains("unknown task `project-hello` in workspace scope")
        );
    }

    #[test]
    fn rejects_json_for_persistent_tasks() {
        let workspace = TestWorkspace::new("gomo-persistent-task");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.server]
persistent = true
steps = [{ exec = { program = "server" } }]
"#,
        );

        let error = run(
            workspace.path(),
            TaskRunRequest {
                name: "server".to_string(),
                project: None,
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::disabled(),
            OutputOptions {
                json: true,
                ..OutputOptions::default()
            },
        )
        .expect_err("persistent JSON should fail before execution");

        assert!(error.to_string().contains("--json is not supported"));
    }

    #[test]
    fn step_identity_names_dev_and_project_tasks() {
        use crate::task::{DevAction, TaskAction};
        let dev = TaskStep {
            dev: Some(DevAction {
                project: "api".to_string(),
                command: Vec::new(),
            }),
            task: None,
            target: None,
            watch: None,
            module: None,
            exec: None,
            shell: None,
        };
        let task = TaskStep {
            task: Some(TaskAction {
                name: "vite-dev".to_string(),
                project: Some("web".to_string()),
            }),
            dev: None,
            target: None,
            watch: None,
            module: None,
            exec: None,
            shell: None,
        };
        assert_eq!(step_identity(&dev, 0), "api:dev");
        assert_eq!(step_identity(&task, 1), "web:vite-dev");
    }

    #[test]
    fn persistent_parallel_shells_demux_logs_without_tui() {
        let workspace = TestWorkspace::new("gomo-persistent-surface");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.stack]
persistent = true
mode = "parallel"
steps = [
  { shell = { command = "printf 'alpha\\n'; sleep 0.2" } },
  { shell = { command = "printf 'beta\\n'; sleep 0.2" } },
]
"#,
        );

        // Finite shells marked persistent will fail on status 0; use loops cancelled by timeout path.
        // Instead use exec that fails after printing so steps end.
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.stack]
persistent = true
mode = "parallel"
steps = [
  { shell = { command = "printf 'alpha\\n'; exit 1" } },
  { shell = { command = "printf 'beta\\n'; exit 1" } },
]
"#,
        );

        let error = run(
            workspace.path(),
            TaskRunRequest {
                name: "stack".to_string(),
                project: None,
                parallelism: Parallelism::Fixed(2),
            },
            CacheOptions::disabled(),
            OutputOptions {
                tui: false,
                ci: true,
                ..OutputOptions::default()
            },
        )
        .expect_err("persistent shells exiting non-zero should fail the aggregate");

        let message = error.to_string();
        assert!(
            message.contains("failed") || message.contains("exit"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn project_task_inputs_can_include_workspace_rooted_paths() {
        let workspace = TestWorkspace::new("gomo-project-workspace-inputs");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.bundle]
scope = "project"
cache = true
inputs = [
  "src/**",
  "{workspace_root}/tooling/shared.txt",
]
outputs = ["out.txt"]
steps = [{ shell = { command = "cat src/app.txt \"$GOMO_WORKSPACE_ROOT/tooling/shared.txt\" > out.txt" } }]
"#,
        );
        workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.tasks.bundle]
extends = "bundle"
"#,
        );
        workspace.write_file("apps/demo/src/app.txt", "app\n");
        workspace.write_file("tooling/shared.txt", "shared\n");

        run(
            workspace.path(),
            TaskRunRequest {
                name: "bundle".to_string(),
                project: Some("demo".to_string()),
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions {
                no_cache: false,
                no_restore: false,
                ..CacheOptions::default()
            },
            OutputOptions::default(),
        )
        .expect("project task should hash workspace-rooted inputs");

        let discovered =
            workspace::discover_from(workspace.path()).expect("workspace should discover");
        let task = discovered
            .projects
            .iter()
            .find(|project| project.name == "demo")
            .expect("demo project")
            .gomo_tasks
            .get("bundle")
            .expect("bundle task");
        let inputs =
            named_task_input_files(&discovered, task, Some("demo")).expect("inputs should resolve");
        let relative = inputs
            .iter()
            .map(|path| {
                path.strip_prefix(workspace.path())
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        assert!(relative.iter().any(|path| path == "apps/demo/src/app.txt"));
        assert!(relative.iter().any(|path| path == "tooling/shared.txt"));

        let missing = named_task_missing_inputs(&discovered, task, Some("demo"), &relative)
            .expect("missing inputs should resolve");
        assert!(
            missing.is_empty(),
            "workspace-rooted inputs should match: {missing:?}"
        );
    }

    #[test]
    fn cached_tasks_restore_declared_outputs() {
        let workspace = TestWorkspace::new("gomo-cached-task");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.generate]
cache = true
inputs = ["input.txt"]
outputs = ["output.txt"]
steps = [{ shell = { command = "cp input.txt output.txt" } }]
"#,
        );
        workspace.write_file("input.txt", "generated\n");

        let request = || TaskRunRequest {
            name: "generate".to_string(),
            project: None,
            parallelism: Parallelism::Fixed(1),
        };
        run(
            workspace.path(),
            request(),
            CacheOptions {
                no_cache: false,
                no_restore: false,
                ..CacheOptions::default()
            },
            OutputOptions::default(),
        )
        .expect("first task run should populate the cache");
        fs::remove_file(workspace.path().join("output.txt")).expect("output should be removable");

        let output = run(
            workspace.path(),
            request(),
            CacheOptions {
                no_cache: false,
                no_restore: false,
                ..CacheOptions::default()
            },
            OutputOptions::default(),
        )
        .expect("second task run should restore the cache");

        assert!(output.stdout.contains("(cached)"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("output.txt")).expect("output was restored"),
            "generated\n"
        );
    }

    #[test]
    fn named_tasks_export_and_import_the_shared_remote_bundle_format() {
        let workspace = TestWorkspace::new("gomo-named-task-bundle");
        workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*"]

[tasks.generate]
cache = true
inputs = ["input.txt"]
outputs = ["output.txt"]
env_inputs = ["GOMO_NAMED_BUNDLE_TEST"]
steps = [{ shell = { command = "cp input.txt output.txt" } }]
"#,
        );
        workspace.write_file("input.txt", "generated\n");
        run(
            workspace.path(),
            TaskRunRequest {
                name: "generate".to_string(),
                project: None,
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::default(),
            OutputOptions::default(),
        )
        .expect("named task should populate its cache");

        let discovered =
            workspace::discover_from(workspace.path()).expect("workspace should discover");
        let task = discovered.tasks.get("generate").expect("task should exist");
        let entry = named_cache_entry(&discovered, task, None).expect("cache entry should resolve");
        let descriptor = named_task_descriptor(&discovered, task, None, &entry)
            .expect("descriptor should resolve");
        let bundle = cache::export_named_task_bundle(&discovered, &descriptor)
            .expect("named bundle should export");
        fs::remove_dir_all(&entry).expect("local entry should be removable");

        cache::import_named_task_bundle(
            &discovered,
            &descriptor,
            &bundle.path,
            &bundle.bundle_digest,
            bundle.byte_len,
            None,
        )
        .expect("named bundle should import");
        assert!(
            cache::validate_named_task_entry(&entry, &descriptor)
                .expect("imported entry should validate")
        );
        assert!(
            descriptor
                .hash_manifest
                .inputs
                .iter()
                .any(|input| input.path == "input.txt")
        );
        assert!(
            descriptor
                .hash_manifest
                .environment
                .iter()
                .any(|input| input.name == "GOMO_NAMED_BUNDLE_TEST")
        );
        fs::remove_file(bundle.path).expect("bundle should be removable");
    }

    #[test]
    fn run_many_reuses_discovered_workspace() {
        let workspace = workspace_with_tasks();

        let output = run_many(
            workspace.path(),
            TaskRunManyRequest {
                name: "project-hello".to_string(),
                all: true,
                projects: Vec::new(),
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::disabled(),
            OutputOptions::default(),
        )
        .expect("run-many should succeed");

        assert!(
            output.stdout.contains(
                workspace
                    .path()
                    .join("apps/demo")
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }
}
