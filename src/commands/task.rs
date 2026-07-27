use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

thread_local! {
    static TARGET_TASK_STACK: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn target_task_stack_contains(key: &(String, String)) -> bool {
    TARGET_TASK_STACK.with(|stack| stack.borrow().iter().any(|entry| entry == key))
}

pub(crate) fn push_target_task_stack(key: (String, String)) {
    TARGET_TASK_STACK.with(|stack| stack.borrow_mut().push(key));
}

pub(crate) fn pop_target_task_stack() {
    TARGET_TASK_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
}

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use walkdir::WalkDir;

use crate::cache::CACHE_SCHEMA_VERSION;
use crate::commands::{CommandOutput, OutputOptions};
use crate::runner::{CommandOptions, Target};
use crate::task::{ExecAction, ShellAction, Task, TaskMode, TaskScope, TaskStep};
use crate::workspace::{self, Project, Workspace};

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
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
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
    render_values(&mut output, &task.inputs);
    output.push_str("Matched Inputs:\n");
    render_values(&mut output, &inputs);
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
    let cached = task.cache.then_some("  cached").unwrap_or("");
    let persistent = task.persistent.then_some("  persistent").unwrap_or("");
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
        output_options,
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
            output_options,
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
            output_options,
            state,
        )?);
    }

    let cache_entry = if task.cache && !cache_options.no_cache {
        let entry = named_cache_entry(workspace, task, project_name)?;
        if named_cache_hit(
            workspace,
            task,
            project_name,
            &entry,
            cache_options.no_restore,
        )? {
            state
                .lock()
                .map_err(|_| anyhow!("task completion state was poisoned"))?
                .completed
                .insert(identity.clone());
            return Ok(format!("✓ {identity} (cached)\n"));
        }
        Some(entry)
    } else {
        None
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
                    output_options,
                    state,
                )?);
            }
        }
        TaskMode::Parallel => {
            let results = thread::scope(|scope| {
                let mut handles = Vec::new();
                for step in &task.steps {
                    handles.push(scope.spawn(|| {
                        execute_step(
                            workspace,
                            task,
                            step,
                            project_name,
                            parallelism,
                            cache_options,
                            output_options,
                            state,
                        )
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
    state
        .lock()
        .map_err(|_| anyhow!("task completion state was poisoned"))?
        .completed
        .insert(identity);
    if let Some(cache_entry) = cache_entry {
        store_named_cache_entry(workspace, task, project_name, &cache_entry)?;
    }
    Ok(output)
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
    if let Some(action) = &step.task {
        let project = action.project.as_deref().or(current_project);
        let task = select_task(workspace, &action.name, project)?;
        return execute_task(
            workspace,
            task,
            project,
            parallelism,
            cache_options,
            output_options,
            state,
        );
    }
    if let Some(action) = &step.target {
        let project = required_project(action.project.as_deref().or(current_project), owner)?;
        let target = parse_target(&action.target)?;
        return native_output(super::run::run(
            &workspace.root,
            RunRequest {
                target,
                command_options: CommandOptions::default(),
                selection: ProjectSelection::Project(project.to_string()),
                with_deps: action.with_deps,
                parallelism,
            },
            cache_options,
            output_options,
        )?);
    }
    if let Some(action) = &step.dev {
        return native_output(super::dev::run(
            &workspace.root,
            DevRequest {
                project: action.project.clone(),
                command: action.command.clone(),
                debounce: Duration::from_millis(300),
                reload: None,
                parallelism,
            },
            cache_options,
            output_options,
        )?);
    }
    if let Some(action) = &step.watch {
        let target = parse_target(&action.target)?;
        return native_output(super::watch::run(
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
            output_options,
        )?);
    }
    if let Some(action) = &step.module {
        let project = find_project(workspace, &action.project)?;
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
        );
    }
    if let Some(action) = &step.exec {
        let cwd = action_cwd(workspace, current_project, action.cwd.as_deref())?;
        let project_root = current_project
            .map(|name| find_project(workspace, name).map(|project| project.root.as_path()))
            .transpose()?;
        return run_exec(workspace, owner, &cwd, project_root, action);
    }
    if let Some(action) = &step.shell {
        let cwd = action_cwd(workspace, current_project, action.cwd.as_deref())?;
        let project_root = current_project
            .map(|name| find_project(workspace, name).map(|project| project.root.as_path()))
            .transpose()?;
        return run_shell(workspace, owner, &cwd, project_root, action);
    }
    unreachable!("task steps are validated during discovery")
}

fn native_output(output: CommandOutput) -> Result<String> {
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
    )
}

fn run_shell(
    workspace: &Workspace,
    owner: &Task,
    cwd: &Path,
    project_root: Option<&Path>,
    action: &ShellAction,
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
    )
}

fn run_process(
    workspace: &Workspace,
    owner: &Task,
    cwd: &Path,
    project_root: Option<&Path>,
    program: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    interpolate_args: bool,
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
    let output = command
        .output()
        .with_context(|| format!("failed to run `{program}` in {}", cwd.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
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
    for dependency in &task.depends_on {
        let dependency_task = select_task(workspace, dependency, project_name)?;
        let dependency_entry = named_cache_entry(workspace, dependency_task, project_name)?;
        hasher.update(dependency_entry.to_string_lossy().as_bytes());
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
        .map(|project| format!("{project}-{}", task.name))
        .unwrap_or_else(|| task.name.clone());
    Ok(workspace
        .cache_dir
        .join(CACHE_SCHEMA_VERSION)
        .join("named-task")
        .join(identity)
        .join(hasher.finalize().to_hex().as_str()))
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
        patterns.push("dev/**".to_string());
    }
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = workspace_relative_pattern(workspace, project, &pattern)?;
        builder.add(
            Glob::new(&pattern).with_context(|| {
                format!("invalid input glob `{pattern}` for task `{}`", task.name)
            })?,
        );
    }
    let globs = builder.build()?;
    let walk_root = project
        .map(|project| project.root.as_path())
        .unwrap_or(workspace.root.as_path());
    let mut files = WalkDir::new(walk_root)
        .into_iter()
        .filter_entry(|entry| {
            // Only prune ignored metadata directories at the walk root, not nested
            // paths such as `src/build` that may match declared input globs.
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".jj" | ".gomo" | "build" | "target" | "node_modules")
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
) -> Result<bool> {
    if !entry.join("success").is_file() {
        return Ok(false);
    }
    let outputs = named_output_paths(workspace, task, project_name)?;
    if outputs.is_empty() {
        return Ok(true);
    }
    let archive = entry.join("outputs.tar.zst");
    if no_restore {
        return Ok(outputs.iter().all(|output| output.exists()));
    }
    if !archive.is_file() {
        return Ok(false);
    }
    restore_named_outputs(workspace, &outputs, &archive)?;
    Ok(true)
}

fn store_named_cache_entry(
    workspace: &Workspace,
    task: &Task,
    project_name: Option<&str>,
    entry: &Path,
) -> Result<()> {
    fs::create_dir_all(entry)
        .with_context(|| format!("failed to create task cache {}", entry.display()))?;
    let outputs = named_output_paths(workspace, task, project_name)?;
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
        let archive_file = File::create(entry.join("outputs.tar.zst"))?;
        let encoder = zstd::stream::write::Encoder::new(archive_file, 0)?;
        let mut archive = tar::Builder::new(encoder);
        for output in &outputs {
            let relative = output.strip_prefix(&workspace.root)?;
            if output.is_dir() {
                archive.append_dir_all(relative, output)?;
            } else {
                archive.append_path_with_name(output, relative)?;
            }
        }
        archive.into_inner()?.finish()?;
    }
    fs::write(entry.join("success"), b"ok\n")?;
    Ok(())
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
        for entry in archive_reader.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.is_absolute()
                || path.components().any(|component| {
                    !matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
                || !allowed.iter().any(|output| path.starts_with(output))
            {
                bail!(
                    "cached named-task archive contains unsafe path {}",
                    path.display()
                );
            }
            if entry.header().entry_type().is_symlink()
                || entry.header().entry_type().is_hard_link()
            {
                bail!(
                    "cached named-task archive contains unsupported link at {}",
                    path.display()
                );
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
