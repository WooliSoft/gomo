use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use watchexec::Watchexec;
use watchexec::command::{Command, Program, Shell, SpawnOptions};
use watchexec::job::{Job, start_job};
use watchexec_events::{Event, KeyCode, Keyboard, Tag};
use watchexec_signals::Signal;

use crate::affected;
use crate::cancellation::CancellationToken;
use crate::commands::{CommandOutput, OutputOptions};
use crate::gleam_toml::GomoReloadStrategy;
use crate::graph::ProjectGraph;
use crate::runner::{CommandOptions, GleamCommandRunner, Target};
use crate::ui::surface::{self, RenderSurface};
use crate::workspace::{self, Project, Workspace};

use super::run::{self, CacheOptions, Parallelism, ProjectSelection, RunRequest};
use super::watch_support::normalize_paths;

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevRequest {
    pub(crate) project: String,
    pub(crate) command: Vec<String>,
    pub(crate) debounce: Duration,
    pub(crate) reload: Option<GomoReloadStrategy>,
    pub(crate) parallelism: Parallelism,
}

struct DevBatch {
    events: Vec<Event>,
    stopped: bool,
}

enum DevBatchAction {
    None,
    Restart,
    Recreate(Command),
}

pub(crate) fn run(
    cwd: &Path,
    request: DevRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let project = find_project(&workspace, &request.project)?.clone();
    let build_cancellation = output_options
        .surface
        .as_ref()
        .map(RenderSurface::cancellation)
        .unwrap_or_default();
    let build_result = build_project(
        &workspace,
        &graph,
        &project,
        cache_options,
        request.parallelism,
        output_options.clone(),
        build_cancellation.clone(),
    );
    if build_cancellation.is_cancelled() {
        return Ok(CommandOutput::success(String::new()));
    }
    build_result?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize the async runtime for gomo dev")?;
    if output_options.surface.is_some() {
        let result = runtime.block_on(run_async_surface(
            cwd.to_path_buf(),
            request,
            cache_options,
            output_options,
            workspace,
            graph,
            project,
        ));
        runtime.shutdown_background();
        return result;
    }
    runtime.block_on(run_async(
        cwd.to_path_buf(),
        request,
        cache_options,
        output_options,
        workspace,
        graph,
        project,
    ))
}

async fn run_async(
    cwd: PathBuf,
    request: DevRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    mut workspace: Workspace,
    mut graph: ProjectGraph,
    mut project: Project,
) -> Result<CommandOutput> {
    let command = development_command(&project, &request)?;
    report(&format!(
        "gomo dev: starting {} in {}\n",
        command.program,
        project.root.display()
    ));
    let (mut job, mut job_task) = start_job(Arc::new(command));
    job.start().await;

    let cancellation = CancellationToken::new();
    let action_cancellation = cancellation.clone();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let wx = Watchexec::new(move |mut action| {
        let stopped = action.signals().any(|signal| {
            matches!(
                signal,
                Signal::Interrupt | Signal::Terminate | Signal::ForceStop
            )
        }) || action.events.iter().any(is_quit_event);
        if stopped {
            action_cancellation.cancel();
        }
        let _ = sender.send(DevBatch {
            events: action.events.iter().cloned().collect(),
            stopped,
        });
        if stopped {
            action.quit();
        }
        action
    })
    .map_err(|error| anyhow!("failed to initialize Watchexec: {error}"))?;
    wx.config.pathset([workspace.root.clone()]);
    wx.config.throttle(request.debounce);
    wx.config.keyboard_events(true);
    let mut watcher_task = wx.main();
    let mut process_wait = job.to_wait();

    let result = loop {
        tokio::select! {
            watcher_result = &mut watcher_task => {
                stop_job(&job).await;
                let watcher_result = watcher_result.context("Watchexec task failed")?;
                watcher_result.map_err(|error| anyhow!("Watchexec failed: {error}"))?;
                break Ok(CommandOutput::success(String::new()));
            }
            _ = &mut process_wait => {
                watcher_task.abort();
                break Ok(CommandOutput::with_exit_code(
                    "development process exited\n".to_string(),
                    1,
                ));
            }
            batch = receiver.recv() => {
                let Some(batch) = batch else {
                    stop_job(&job).await;
                    break Err(anyhow!("Watchexec event channel stopped unexpectedly"));
                };
                if batch.stopped {
                    stop_job(&job).await;
                    break Ok(CommandOutput::success(String::new()));
                }
                match handle_batch(
                    &cwd,
                    &request,
                    &cache_options,
                    &mut workspace,
                    &mut graph,
                    &mut project,
                    &batch,
                    output_options.clone(),
                    &cancellation,
                ).await? {
                    DevBatchAction::None => {}
                    DevBatchAction::Restart => {
                        report_restart(&job);
                        job.restart_with_signal(Signal::Terminate, SHUTDOWN_GRACE_PERIOD)
                            .await;
                        process_wait = job.to_wait();
                    }
                    DevBatchAction::Recreate(command) => {
                        report_restart(&job);
                        stop_job(&job).await;
                        let (next_job, next_job_task) = start_job(Arc::new(command));
                        let previous_job = std::mem::replace(&mut job, next_job);
                        drop(previous_job);
                        let _ = (&mut job_task).await;
                        job_task = next_job_task;
                        job.start().await;
                        process_wait = job.to_wait();
                    }
                }
            }
        }
    };

    drop(process_wait);
    drop(job);
    let _ = job_task.await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_batch(
    cwd: &Path,
    request: &DevRequest,
    cache_options: &CacheOptions,
    workspace: &mut Workspace,
    graph: &mut ProjectGraph,
    project: &mut Project,
    batch: &DevBatch,
    output_options: OutputOptions,
    cancellation: &CancellationToken,
) -> Result<DevBatchAction> {
    let (paths, structural) = normalize_paths(workspace, &batch.events);
    if paths.is_empty() && !structural {
        return Ok(DevBatchAction::None);
    }

    let (next_workspace, next_graph, next_project) = if structural {
        rediscover(cwd, &request.project)?
    } else {
        (workspace.clone(), graph.clone(), project.clone())
    };
    let affected = if structural && paths.iter().any(|path| path == Path::new("gomo.toml")) {
        vec![request.project.clone()]
    } else {
        affected::select_affected_projects_within(
            &next_workspace,
            &next_graph,
            Target::Build,
            &paths,
            Some(&upstream_universe(
                &next_workspace,
                &next_graph,
                &request.project,
            )?),
        )?
    };
    if affected.is_empty() {
        return Ok(DevBatchAction::None);
    }

    let build_cache_options = *cache_options;
    let build_parallelism = request.parallelism;
    let build_cancellation = cancellation.clone();
    let build_result = tokio::task::spawn_blocking(move || {
        build_project(
            &next_workspace,
            &next_graph,
            &next_project,
            build_cache_options,
            build_parallelism,
            output_options,
            build_cancellation,
        )
        .map(|()| (next_workspace, next_graph, next_project))
    })
    .await
    .context("gomo dev build worker failed")?;

    match build_result {
        Ok((next_workspace, next_graph, next_project)) => {
            let recreate = should_recreate_job(project, &next_project, request);
            let next_command = recreate
                .then(|| development_command(&next_project, request))
                .transpose()?;
            *workspace = next_workspace;
            *graph = next_graph;
            *project = next_project;
            if matches!(
                request.reload.or(Some(project.gomo_dev.reload)),
                Some(GomoReloadStrategy::Hot)
            ) {
                report("gomo dev: hot reload helper unavailable; restarting\n");
            }
            Ok(next_command.map_or(DevBatchAction::Restart, DevBatchAction::Recreate))
        }
        Err(error) => {
            if cancellation.is_cancelled() {
                return Ok(DevBatchAction::None);
            }
            report(&format!(
                "gomo dev: build failed; keeping the current process: {error}\n"
            ));
            Ok(DevBatchAction::None)
        }
    }
}

fn should_recreate_job(current: &Project, next: &Project, request: &DevRequest) -> bool {
    current.root != next.root
        || (request.command.is_empty() && current.gomo_dev.command != next.gomo_dev.command)
}

fn report_restart(job: &Job) {
    if !job.is_running() {
        report("gomo dev: development process exited; restarting\n");
    } else {
        report("gomo dev: restarting development process\n");
    }
}

async fn stop_job(job: &Job) {
    job.stop_with_signal(Signal::Terminate, SHUTDOWN_GRACE_PERIOD)
        .await;
}

fn development_command(project: &Project, request: &DevRequest) -> Result<Command> {
    let (program, args) = if !request.command.is_empty() {
        (request.command[0].clone(), request.command[1..].to_vec())
    } else if let Some(command) = project.gomo_dev.command.as_deref() {
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    } else {
        ("gleam".to_string(), vec!["run".to_string()])
    };
    if program.trim().is_empty() {
        bail!("development command must not be empty");
    }
    let command = format!(
        "cd {} && exec {}",
        shell_quote(&project.root),
        std::iter::once(program.as_str())
            .chain(args.iter().map(String::as_str))
            .map(shell_quote_str)
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(Command {
        program: Program::Shell {
            shell: Shell::new("sh"),
            command,
            args: Vec::new(),
        },
        options: SpawnOptions {
            grouped: true,
            session: true,
            ..Default::default()
        },
    })
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.to_string_lossy())
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn find_project<'a>(workspace: &'a Workspace, name: &str) -> Result<&'a Project> {
    workspace
        .projects
        .iter()
        .find(|project| project.name == name)
        .with_context(|| format!("unknown project `{name}`"))
}

fn upstream_universe(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project: &str,
) -> Result<BTreeSet<String>> {
    Ok(run::selected_project_names(
        workspace,
        graph,
        &RunRequest {
            target: Target::Build,
            command_options: CommandOptions::default(),
            selection: ProjectSelection::Project(project.to_string()),
            with_deps: true,
            parallelism: Parallelism::WorkspaceDefault,
        },
    )?
    .into_iter()
    .collect())
}

fn rediscover(cwd: &Path, project_name: &str) -> Result<(Workspace, ProjectGraph, Project)> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let project = find_project(&workspace, project_name)?.clone();
    Ok((workspace, graph, project))
}

fn build_project(
    workspace: &Workspace,
    graph: &ProjectGraph,
    project: &Project,
    cache_options: CacheOptions,
    parallelism: Parallelism,
    output_options: OutputOptions,
    cancellation: CancellationToken,
) -> Result<()> {
    let request = RunRequest {
        target: Target::Build,
        command_options: CommandOptions::default(),
        selection: ProjectSelection::Project(project.name.clone()),
        with_deps: true,
        parallelism,
    };
    let project_names = run::selected_project_names(workspace, graph, &request)?;
    let build_cancellation = cancellation.clone();
    let surface = output_options.surface.clone();
    let output = run::run_project_names_with_cancellation(
        workspace,
        graph,
        &project_names,
        Target::Build,
        CommandOptions::default(),
        &GleamCommandRunner,
        cache_options,
        parallelism,
        output_options.without_tui(),
        cancellation,
    )?;
    if build_cancellation.is_cancelled() {
        bail!("build cancelled");
    }
    if !output.stdout.is_empty() {
        report_to(&output.stdout, surface.as_ref());
    }
    if output.exit_code != 0 {
        bail!("build exited with code {}", output.exit_code);
    }
    Ok(())
}

fn is_quit_event(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        matches!(
            tag,
            Tag::Keyboard(Keyboard::Key {
                key: KeyCode::Char('q' | 'Q'),
                modifiers,
            }) if modifiers.is_empty()
        )
    })
}

fn report(message: &str) {
    print!("{message}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn report_to(message: &str, surface: Option<&RenderSurface>) {
    if let Some(surface) = surface {
        surface.append_log(message);
    } else {
        report(message);
    }
}

/// Dev under a parent persistent TUI: pipe the process and never open nested UI.
async fn run_async_surface(
    cwd: PathBuf,
    request: DevRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    mut workspace: Workspace,
    mut graph: ProjectGraph,
    mut project: Project,
) -> Result<CommandOutput> {
    let surface = output_options
        .surface
        .clone()
        .context("surface dev path requires a render surface")?;
    let session_cancel = surface.cancellation();

    let mut process_cancel = CancellationToken::new();
    let (mut process_handle, mut process_result) =
        spawn_surface_dev_process(&project, &request, &surface, &process_cancel);

    let action_cancellation = session_cancel.clone();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let wx = Watchexec::new(move |mut action| {
        let stopped = action.signals().any(|signal| {
            matches!(
                signal,
                Signal::Interrupt | Signal::Terminate | Signal::ForceStop
            )
        }) || action.events.iter().any(is_quit_event);
        if stopped {
            action_cancellation.cancel();
        }
        let _ = sender.send(DevBatch {
            events: action.events.iter().cloned().collect(),
            stopped,
        });
        if stopped {
            action.quit();
        }
        action
    })
    .map_err(|error| anyhow!("failed to initialize Watchexec: {error}"))?;
    wx.config.pathset([workspace.root.clone()]);
    wx.config.throttle(request.debounce);
    // The parent persistent-task terminal owns keyboard input.
    wx.config.keyboard_events(false);
    let mut watcher_task = wx.main();

    loop {
        tokio::select! {
            biased;

            result = &mut process_result => {
                watcher_task.abort();
                let join_result = process_handle.join();
                return surface_dev_exit(result, join_result);
            }
            _ = tokio::time::sleep(Duration::from_millis(40)) => {
                if session_cancel.is_cancelled() {
                    process_cancel.cancel();
                    let _ = process_handle.join();
                    watcher_task.abort();
                    return Ok(CommandOutput::success(String::new()));
                }
            }
            watcher_result = &mut watcher_task => {
                process_cancel.cancel();
                let _ = process_handle.join();
                let watcher_result = watcher_result.context("Watchexec task failed")?;
                watcher_result.map_err(|error| anyhow!("Watchexec failed: {error}"))?;
                return Ok(CommandOutput::success(String::new()));
            }
            batch = receiver.recv() => {
                let Some(batch) = batch else {
                    process_cancel.cancel();
                    let _ = process_handle.join();
                    watcher_task.abort();
                    bail!("Watchexec event channel stopped unexpectedly");
                };
                if batch.stopped || session_cancel.is_cancelled() {
                    process_cancel.cancel();
                    let _ = process_handle.join();
                    watcher_task.abort();
                    return Ok(CommandOutput::success(String::new()));
                }
                match handle_batch(
                    &cwd,
                    &request,
                    &cache_options,
                    &mut workspace,
                    &mut graph,
                    &mut project,
                    &batch,
                    output_options.clone(),
                    &session_cancel,
                ).await? {
                    DevBatchAction::None => {}
                    DevBatchAction::Restart | DevBatchAction::Recreate(_) => {
                        report_to("gomo dev: restarting development process\n", Some(&surface));
                        process_cancel.cancel();
                        let _ = process_handle.join();
                        if session_cancel.is_cancelled() {
                            watcher_task.abort();
                            return Ok(CommandOutput::success(String::new()));
                        }
                        process_cancel = CancellationToken::new();
                        (process_handle, process_result) = spawn_surface_dev_process(
                            &project,
                            &request,
                            &surface,
                            &process_cancel,
                        );
                    }
                }
            }
        }
    }
}

fn spawn_surface_dev_process(
    project: &Project,
    request: &DevRequest,
    surface: &RenderSurface,
    process_cancel: &CancellationToken,
) -> (
    std::thread::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<Result<()>>,
) {
    let project = project.clone();
    let request = request.clone();
    let linked = link_cancellation(surface.cancellation(), process_cancel.clone());
    let surface = surface.with_cancellation(linked);
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let handle = std::thread::spawn(move || {
        let (program, args) = if !request.command.is_empty() {
            (request.command[0].clone(), request.command[1..].to_vec())
        } else if let Some(command) = project.gomo_dev.command.as_deref() {
            (
                "sh".to_string(),
                vec!["-c".to_string(), command.to_string()],
            )
        } else {
            ("gleam".to_string(), vec!["run".to_string()])
        };
        let mut command = std::process::Command::new(&program);
        command.args(&args).current_dir(&project.root);
        let result = match surface::run_piped_persistent(command, &surface, || {
            "development process exited".to_string()
        }) {
            Ok(_) => Ok(()),
            Err(_error) if surface.cancellation().is_cancelled() => Ok(()),
            Err(error) if error.to_string().contains("development process exited") => {
                // Unexpected exit — treat as failure so the parent step stops siblings.
                Err(error)
            }
            Err(error) => Err(error),
        };
        let _ = result_tx.send(result);
    });
    (handle, result_rx)
}

fn surface_dev_exit(
    result: std::result::Result<Result<()>, tokio::sync::oneshot::error::RecvError>,
    join_result: std::thread::Result<()>,
) -> Result<CommandOutput> {
    if join_result.is_err() {
        bail!("development process worker panicked");
    }
    match result {
        Ok(Ok(())) => bail!("development process exited"),
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("development process result channel stopped unexpectedly"),
    }
}

fn link_cancellation(left: CancellationToken, right: CancellationToken) -> CancellationToken {
    let linked = CancellationToken::new();
    let signal = linked.clone();
    std::thread::spawn(move || {
        while !left.is_cancelled() && !right.is_cancelled() {
            std::thread::sleep(Duration::from_millis(40));
        }
        signal.cancel();
    });
    linked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    fn dev_project() -> Project {
        let test_workspace = TestWorkspace::new("gomo-dev-project");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.dev]
command = "gleam run"
"#,
        );
        workspace::discover(test_workspace.path())
            .expect("workspace should load")
            .projects
            .into_iter()
            .next()
            .expect("project should exist")
    }

    fn request(command: Vec<String>) -> DevRequest {
        DevRequest {
            project: "demo".to_string(),
            command,
            debounce: Duration::from_millis(100),
            reload: None,
            parallelism: Parallelism::WorkspaceDefault,
        }
    }

    #[test]
    fn configured_command_changes_recreate_the_development_job() {
        let current = dev_project();
        let mut next = current.clone();
        next.gomo_dev.command = Some("gleam run -m changed".to_string());

        assert!(should_recreate_job(&current, &next, &request(Vec::new())));
        assert!(!should_recreate_job(
            &current,
            &next,
            &request(vec!["gleam".to_string(), "run".to_string()])
        ));
    }

    #[test]
    fn project_root_changes_recreate_the_development_job() {
        let current = dev_project();
        let mut next = current.clone();
        next.root.push("moved");

        assert!(should_recreate_job(&current, &next, &request(Vec::new())));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_development_rebuild() {
        let test_workspace = TestWorkspace::new("gomo-dev-build-cancel");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
command = "sleep 10"
"#,
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let graph = ProjectGraph::build(&workspace).expect("graph should build");
        let project = find_project(&workspace, "demo").expect("project should exist");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let started = std::time::Instant::now();

        let result = build_project(
            &workspace,
            &graph,
            project,
            CacheOptions::disabled(),
            Parallelism::Fixed(1),
            OutputOptions::default(),
            cancellation,
        );

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn completed_surface_dev_process_is_an_error() {
        let error = surface_dev_exit(Ok(Ok(())), Ok(()))
            .expect_err("an unexpected development process exit should fail");

        assert_eq!(error.to_string(), "development process exited");
    }

    #[test]
    fn surface_dev_process_error_is_preserved() {
        let error = surface_dev_exit(Ok(Err(anyhow!("address already in use"))), Ok(()))
            .expect_err("a development process error should fail");

        assert_eq!(error.to_string(), "address already in use");
    }

    #[cfg(unix)]
    #[test]
    fn surface_dev_returns_when_process_exits() {
        let test_workspace = TestWorkspace::new("gomo-surface-dev-exit");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
command = "true"
"#,
        );
        let cancellation = CancellationToken::new();
        let (event_tx, _event_rx, _) = RenderSurface::channel(cancellation.clone(), false);
        let surface = RenderSurface::new("demo:dev", event_tx, cancellation, false);

        let error = run(
            test_workspace.path(),
            DevRequest {
                project: "demo".to_string(),
                command: vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
                debounce: Duration::from_millis(10),
                reload: None,
                parallelism: Parallelism::Fixed(1),
            },
            CacheOptions::disabled(),
            OutputOptions {
                surface: Some(surface),
                ..OutputOptions::default()
            },
        )
        .expect_err("an exited development process should fail");

        assert!(error.to_string().contains("failed with exit code 7"));
    }
}
