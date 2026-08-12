use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    Event as TerminalEvent, EventStream, KeyCode as TuiKeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::{
    cursor, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use watchexec::Watchexec;
use watchexec_events::Event;
use watchexec_signals::Signal;

use crate::affected;
use crate::cancellation::{CancellationToken, configure_process_group, wait_for_child};
use crate::commands::{CommandOutput, OutputOptions};
use crate::graph::ProjectGraph;
use crate::runner::{CommandOptions, Target};
use crate::workspace::{self, Project, Workspace};

use super::run::{self, CacheOptions, Parallelism, ProjectSelection, RunRequest};
use super::watch_support::normalize_paths;

const WATCH_SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const WATCH_SPINNER_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchRequest {
    pub(crate) target: Option<Target>,
    pub(crate) selection: ProjectSelection,
    pub(crate) with_deps: bool,
    pub(crate) initial_run: bool,
    pub(crate) debounce: Duration,
    pub(crate) callback: Vec<String>,
    pub(crate) parallelism: Parallelism,
}

struct WatchBatch {
    events: Vec<Event>,
    stopped: bool,
}

pub(crate) fn run(
    cwd: &Path,
    request: WatchRequest,
    cache_options: CacheOptions,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let target = resolve_target(&request)?;
    if target == Target::Clean {
        bail!("gomo watch cannot watch the `clean` target");
    }
    let callback = resolve_callback(&workspace, &request)?;
    let interactive_terminal = output_options.tui;
    let _terminal = WatchTerminal::enter(interactive_terminal)?;

    let selected = selected_names(&workspace, &graph, &request, target)?;
    let permitted = upstream_universe(&workspace, &graph, &request, target)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to initialize the async runtime for gomo watch")?;
    let runtime_guard = runtime.enter();
    let (sender, receiver) = mpsc::channel();
    let (tui_session, tui_control) = crate::ui::run::RunManySession::continuous();
    let cancellation = tui_control.cancellation_token();
    let (watcher_ready_sender, watcher_ready_receiver) =
        mpsc::channel::<std::result::Result<(), String>>();
    let stopped = Arc::new(AtomicBool::new(false));
    let action_stopped = Arc::clone(&stopped);
    let action_cancellation = cancellation.clone();
    let input_sender = sender.clone();
    let wx = Watchexec::new(move |mut action| {
        let should_stop = action.signals().any(|signal| {
            matches!(
                signal,
                Signal::Interrupt | Signal::Terminate | Signal::ForceStop
            )
        });
        if should_stop {
            action_stopped.store(true, Ordering::Release);
            action_cancellation.cancel();
        }
        let _ = sender.send(WatchBatch {
            events: action.events.iter().cloned().collect(),
            stopped: should_stop,
        });
        if should_stop {
            action.quit();
        }
        action
    })
    .map_err(|error| anyhow!("failed to initialize Watchexec: {error}"))?;
    let watcher_ready = wx.config.fs_ready();
    wx.config.pathset([workspace.root.clone()]);
    wx.config.throttle(request.debounce);

    let mut main_task = wx.main();
    let ready_task = runtime.spawn(animate_watcher_startup(
        interactive_terminal,
        watcher_ready,
        tui_session.clone(),
        tui_control.clone(),
        watcher_ready_sender,
        request.initial_run,
        selected.clone(),
        target,
        request.parallelism.resolve(workspace.default_parallelism),
        _terminal.alternate_screen_state(),
    ));
    let mut input_task = runtime.spawn(route_terminal_input(
        interactive_terminal,
        input_sender,
        tui_session,
        Arc::clone(&stopped),
    ));
    drop(runtime_guard);
    let cwd = cwd.to_path_buf();
    let mut worker = runtime.spawn_blocking(move || {
        run_loop(
            &cwd,
            request,
            workspace,
            graph,
            target,
            selected,
            permitted,
            callback,
            receiver,
            cache_options,
            output_options,
            tui_control,
            stopped,
            watcher_ready_receiver,
        )
    });

    let result = runtime.block_on(async {
        tokio::select! {
            worker_result = &mut worker => {
                let worker_result = worker_result.context("gomo watch worker failed")?;
                main_task.abort();
                input_task.abort();
                worker_result
            }
            watcher_result = &mut main_task => {
                drop(wx);
                input_task.abort();
                let worker_result = worker.await.context("gomo watch worker failed")?;
                watcher_result
                    .context("Watchexec failed")?
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                worker_result
            }
            input_result = &mut input_task => {
                input_result.context("gomo watch input task failed")??;
                main_task.abort();
                worker.await.context("gomo watch worker failed")?
            }
        }
    });
    ready_task.abort();
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

#[allow(clippy::too_many_arguments)]
async fn animate_watcher_startup(
    interactive: bool,
    mut watcher_ready: tokio::sync::watch::Receiver<()>,
    tui_session: crate::ui::run::RunManySession,
    tui_control: crate::ui::run::RunManyControl,
    ready_sender: mpsc::Sender<std::result::Result<(), String>>,
    initial_run: bool,
    project_names: Vec<String>,
    target: Target,
    parallelism: usize,
    alternate_screen_enabled: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(WATCH_SPINNER_INTERVAL);
    let mut spinner_frame = 0;
    loop {
        tokio::select! {
            ready = watcher_ready.changed() => {
                let result = if ready.is_err() {
                    Err("filesystem watcher stopped before becoming ready".to_string())
                } else if interactive {
                    enter_ready_watch_terminal(
                        &tui_session,
                        tui_control.clone(),
                        initial_run,
                        &project_names,
                        target,
                        parallelism,
                        &alternate_screen_enabled,
                    )
                    .map_err(|error| error.to_string())
                } else {
                    Ok(())
                };
                let _ = ready_sender.send(result);
                return;
            }
            _ = interval.tick(), if interactive => {
                let _ = draw_watch_startup_spinner(spinner_frame);
                spinner_frame = (spinner_frame + 1) % WATCH_SPINNER_FRAMES.len();
            }
        }
    }
}

fn draw_watch_startup_spinner(frame: usize) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    write!(
        stdout,
        "\r\x1b[2K{} Starting filesystem watcher…",
        WATCH_SPINNER_FRAMES[frame % WATCH_SPINNER_FRAMES.len()]
    )?;
    stdout.flush()
}

fn enter_ready_watch_terminal(
    tui_session: &crate::ui::run::RunManySession,
    tui_control: crate::ui::run::RunManyControl,
    initial_run: bool,
    project_names: &[String],
    target: Target,
    parallelism: usize,
    alternate_screen_enabled: &AtomicBool,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    write!(stdout, "\r\x1b[2K")?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    alternate_screen_enabled.store(true, Ordering::Release);
    if !initial_run {
        tui_session.show_waiting(project_names, target, parallelism, tui_control)?;
    }
    Ok(())
}

async fn route_terminal_input(
    interactive: bool,
    watch_sender: mpsc::Sender<WatchBatch>,
    tui_session: crate::ui::run::RunManySession,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    if !interactive {
        std::future::pending::<()>().await;
    }
    let mut events = EventStream::new();
    while let Some(event) = events.next().await {
        let event = event.context("failed to read terminal input for gomo watch")?;
        let TerminalEvent::Key(key) = event else {
            continue;
        };
        if is_watch_quit_key(key) {
            stopped.store(true, Ordering::Release);
            tui_session.cancel();
            let _ = watch_sender.send(WatchBatch {
                events: Vec::new(),
                stopped: true,
            });
            return Ok(());
        }
        if key.kind == KeyEventKind::Press {
            let session = tui_session.clone();
            tokio::task::spawn_blocking(move || session.handle_key(key.code))
                .await
                .context("gomo watch TUI input worker failed")?
                .context("failed to update the idle run TUI")?;
        }
    }
    Ok(())
}

fn is_watch_quit_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (matches!(key.code, TuiKeyCode::Char('q' | 'Q')) && key.modifiers.is_empty()
            || matches!(key.code, TuiKeyCode::Char('c' | 'd'))
                && key.modifiers == KeyModifiers::CONTROL)
}

struct WatchTerminal {
    raw_mode_enabled: bool,
    alternate_screen_enabled: Arc<AtomicBool>,
}

impl WatchTerminal {
    fn enter(interactive: bool) -> Result<Self> {
        let alternate_screen_enabled = Arc::new(AtomicBool::new(false));
        if interactive {
            enable_raw_mode().context("failed to enable raw terminal mode for gomo watch")?;
            let mut stdout = std::io::stdout();
            if let Err(error) = execute!(stdout, cursor::Hide) {
                let _ = disable_raw_mode();
                return Err(error).context("failed to initialize the gomo watch terminal");
            }
        }
        Ok(Self {
            raw_mode_enabled: interactive,
            alternate_screen_enabled,
        })
    }

    fn alternate_screen_state(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alternate_screen_enabled)
    }
}

impl Drop for WatchTerminal {
    fn drop(&mut self) {
        if self.raw_mode_enabled {
            let mut stdout = std::io::stdout();
            if self.alternate_screen_enabled.load(Ordering::Acquire) {
                let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
            } else {
                let _ = write!(stdout, "\r\x1b[2K");
                let _ = execute!(stdout, cursor::Show);
                let _ = stdout.flush();
            }
            let _ = disable_raw_mode();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    cwd: &Path,
    request: WatchRequest,
    mut workspace: Workspace,
    mut graph: ProjectGraph,
    target: Target,
    mut selected: Vec<String>,
    mut permitted: BTreeSet<String>,
    mut callback: Option<Callback>,
    receiver: Receiver<WatchBatch>,
    cache_options: CacheOptions,
    output_options: OutputOptions,
    tui_control: crate::ui::run::RunManyControl,
    stopped: Arc<AtomicBool>,
    watcher_ready: Receiver<std::result::Result<(), String>>,
) -> Result<CommandOutput> {
    while !stopped.load(Ordering::Acquire) {
        match watcher_ready.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(())) => break,
            Ok(Err(error)) => bail!("{error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("filesystem watcher stopped before becoming ready")
            }
        }
    }
    if stopped.load(Ordering::Acquire) {
        return Ok(CommandOutput::success(String::new()));
    }

    if request.initial_run {
        run_cycle(
            &workspace,
            &graph,
            &selected,
            &permitted,
            target,
            callback.as_ref(),
            &[],
            cache_options,
            request.with_deps,
            request.parallelism,
            output_options.clone(),
            tui_control.clone(),
        )?;
    }

    while !stopped.load(Ordering::Acquire) {
        let Ok(batch) = receiver.recv() else {
            break;
        };
        if batch.stopped || stopped.load(Ordering::Acquire) {
            break;
        }
        let (paths, structural) = normalize_paths(&workspace, &batch.events);
        if paths.is_empty() && !structural {
            continue;
        }

        if structural {
            match refresh_workspace(cwd, &request, target) {
                Ok((
                    refreshed_workspace,
                    refreshed_graph,
                    refreshed_selected,
                    refreshed_permitted,
                )) => {
                    workspace = refreshed_workspace;
                    graph = refreshed_graph;
                    selected = refreshed_selected;
                    permitted = refreshed_permitted;
                    match resolve_callback(&workspace, &request) {
                        Ok(refreshed_callback) => callback = refreshed_callback,
                        Err(error) => {
                            report_watch_error(
                                &tui_control,
                                output_options.clone(),
                                format!("callback configuration failed: {error}"),
                            )?;
                            continue;
                        }
                    }
                    let cycle_files = if callback.is_some() {
                        paths.as_slice()
                    } else {
                        &[]
                    };
                    run_cycle(
                        &workspace,
                        &graph,
                        &selected,
                        &permitted,
                        target,
                        callback.as_ref(),
                        cycle_files,
                        cache_options,
                        request.with_deps,
                        request.parallelism,
                        output_options.clone(),
                        tui_control.clone(),
                    )?;
                }
                Err(error) => {
                    report_watch_error(
                        &tui_control,
                        output_options.clone(),
                        format!("graph refresh failed: {error}"),
                    )?;
                }
            }
            continue;
        }

        let affected = affected::select_affected_projects_within(
            &workspace,
            &graph,
            target,
            &paths,
            Some(&permitted),
        )?;
        if affected.is_empty() {
            continue;
        }
        run_cycle(
            &workspace,
            &graph,
            &selected,
            &permitted,
            target,
            callback.as_ref(),
            &paths,
            cache_options,
            request.with_deps,
            request.parallelism,
            output_options.clone(),
            tui_control.clone(),
        )?;
    }

    Ok(CommandOutput::success(String::new()))
}

fn resolve_target(request: &WatchRequest) -> Result<Target> {
    if request.target.is_some() && !request.callback.is_empty() {
        bail!("gomo watch accepts either --target or a callback command, not both");
    }
    if let Some(target) = request.target {
        return Ok(target);
    }
    if !request.callback.is_empty() {
        return Ok(Target::Build);
    }
    Ok(Target::Build)
}

fn resolve_callback(workspace: &Workspace, request: &WatchRequest) -> Result<Option<Callback>> {
    if request.target.is_some() {
        return Ok(None);
    }
    if !request.callback.is_empty() {
        return Ok(Some(Callback::argv(
            request.callback.clone(),
            watch_project_name(&request.selection),
        )?));
    }
    let ProjectSelection::Project(name) = &request.selection else {
        return Ok(None);
    };
    let project = workspace
        .projects
        .iter()
        .find(|project| project.name == *name)
        .with_context(|| format!("unknown project `{name}`"))?;
    let Some(command) = project
        .gomo_targets
        .get("watch")
        .and_then(|config| config.command.as_deref())
    else {
        return Ok(None);
    };
    Ok(Some(Callback::shell(
        command,
        watch_project_name(&request.selection),
    )?))
}

fn watch_project_name(selection: &ProjectSelection) -> Option<String> {
    match selection {
        ProjectSelection::Project(project) => Some(project.clone()),
        ProjectSelection::All | ProjectSelection::Projects(_) => None,
    }
}

fn selected_names(
    workspace: &Workspace,
    graph: &ProjectGraph,
    request: &WatchRequest,
    target: Target,
) -> Result<Vec<String>> {
    run::selected_project_names(
        workspace,
        graph,
        &RunRequest {
            target,
            command_options: CommandOptions::default(),
            selection: request.selection.clone(),
            with_deps: request.with_deps,
            parallelism: request.parallelism,
        },
    )
}

fn upstream_universe(
    workspace: &Workspace,
    graph: &ProjectGraph,
    request: &WatchRequest,
    target: Target,
) -> Result<BTreeSet<String>> {
    Ok(run::selected_project_names(
        workspace,
        graph,
        &RunRequest {
            target,
            command_options: CommandOptions::default(),
            selection: request.selection.clone(),
            with_deps: true,
            parallelism: request.parallelism,
        },
    )?
    .into_iter()
    .collect())
}

fn refresh_workspace(
    cwd: &Path,
    request: &WatchRequest,
    target: Target,
) -> Result<(Workspace, ProjectGraph, Vec<String>, BTreeSet<String>)> {
    let workspace = workspace::discover_from(cwd)?;
    let graph = ProjectGraph::build(&workspace)?;
    let selected = selected_names(&workspace, &graph, request, target)?;
    let permitted = upstream_universe(&workspace, &graph, request, target)?;
    Ok((workspace, graph, selected, permitted))
}

#[allow(clippy::too_many_arguments)]
fn run_cycle(
    workspace: &Workspace,
    graph: &ProjectGraph,
    selected: &[String],
    permitted: &BTreeSet<String>,
    target: Target,
    callback: Option<&Callback>,
    changed_files: &[PathBuf],
    cache_options: CacheOptions,
    with_deps: bool,
    parallelism: Parallelism,
    output_options: OutputOptions,
    tui_control: crate::ui::run::RunManyControl,
) -> Result<()> {
    if let Some(callback) = callback {
        let project = callback.watch_project.as_deref().and_then(|name| {
            workspace
                .projects
                .iter()
                .find(|project| project.name == name)
        });
        let changed_projects = if changed_files.is_empty() {
            Vec::new()
        } else {
            let affected = affected::select_affected_projects_within(
                workspace,
                graph,
                target,
                changed_files,
                Some(permitted),
            )?;
            if affected.is_empty() {
                permitted.iter().cloned().collect()
            } else {
                affected
            }
        };
        let callback_status = callback.run(
            workspace,
            project,
            changed_files,
            &changed_projects,
            &tui_control.cancellation_token(),
        )?;
        let Some(callback_status) = callback_status else {
            return Ok(());
        };
        if output_options.tui {
            tui_control
                .show_waiting(
                    selected,
                    target,
                    parallelism.resolve(workspace.default_parallelism),
                )
                .context("failed to show the callback watch TUI")?;
        }
        if !callback_status.success() {
            report_watch_error(
                &tui_control,
                output_options,
                format!("callback exited with {callback_status}"),
            )?;
        }
        return Ok(());
    }
    let project_names = if changed_files.is_empty() {
        selected.to_vec()
    } else {
        let affected = affected::select_affected_projects_within(
            workspace,
            graph,
            target,
            changed_files,
            Some(permitted),
        )?;
        if with_deps {
            affected
        } else {
            let selected_set = selected.iter().collect::<BTreeSet<_>>();
            affected
                .into_iter()
                .filter(|project| selected_set.contains(project))
                .collect()
        }
    };
    if project_names.is_empty() {
        return Ok(());
    }
    let output = run::run_project_names_with_control(
        workspace,
        graph,
        &project_names,
        target,
        CommandOptions::default(),
        &crate::runner::GleamCommandRunner,
        cache_options,
        parallelism,
        output_options.clone(),
        tui_control,
    )?;
    if !output.stdout.is_empty() && !output_options.tui {
        report(&output.stdout);
    }
    Ok(())
}

fn report(message: &str) {
    print!("{message}");
    let _ = std::io::stdout().flush();
}

fn report_watch_error(
    tui_control: &crate::ui::run::RunManyControl,
    output_options: OutputOptions,
    message: String,
) -> Result<()> {
    if output_options.tui {
        tui_control
            .show_watch_error(message)
            .context("failed to show gomo watch error in the TUI")?;
    } else {
        report(&format!("gomo watch: {message}\n"));
    }
    Ok(())
}

struct Callback {
    program: String,
    args: Vec<String>,
    watch_project: Option<String>,
}

impl Callback {
    fn argv(argv: Vec<String>, watch_project: Option<String>) -> Result<Self> {
        let Some(program) = argv.first() else {
            bail!("callback command must not be empty")
        };
        Ok(Self {
            program: program.clone(),
            args: argv[1..].to_vec(),
            watch_project,
        })
    }

    fn shell(command: &str, watch_project: Option<String>) -> Result<Self> {
        if command.trim().is_empty() {
            bail!("configured watch command must not be empty")
        }
        Ok(Self {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command.to_string()],
            watch_project,
        })
    }

    fn run(
        &self,
        workspace: &Workspace,
        project: Option<&Project>,
        changed_files: &[PathBuf],
        changed_projects: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Option<std::process::ExitStatus>> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.current_dir(project.map_or(&workspace.root, |project| &project.root));
        let files = changed_files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        command
            .env("GOMO_CHANGED_FILES", files.join("\n"))
            .env("GOMO_CHANGED_PROJECTS", changed_projects.join(","))
            .env("GOMO_CHANGED_FILES_JSON", serde_json::to_string(&files)?)
            .env(
                "GOMO_CHANGED_PROJECTS_JSON",
                serde_json::to_string(changed_projects)?,
            )
            .env(
                "GOMO_WATCH_PROJECT",
                self.watch_project
                    .clone()
                    .or_else(|| project.map(|project| project.name.clone()))
                    .unwrap_or_default(),
            );
        configure_process_group(&mut command);
        let mut child = command.spawn().context("failed to run watch callback")?;
        let status = wait_for_child(&mut child, cancellation)
            .context("failed to wait for watch callback")?;
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        Ok(Some(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn watch_quit_keys_include_bare_q_and_control_c_but_not_escape() {
        assert!(is_watch_quit_key(KeyEvent::new(
            TuiKeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(is_watch_quit_key(KeyEvent::new(
            TuiKeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_watch_quit_key(KeyEvent::new(
            TuiKeyCode::Esc,
            KeyModifiers::NONE
        )));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_running_watch_callback() {
        let test_workspace = TestWorkspace::new("gomo-watch-callback-cancel");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let callback =
            Callback::shell("sleep 10", Some("demo".to_string())).expect("callback should parse");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let started = std::time::Instant::now();

        callback
            .run(&workspace, None, &[], &[], &cancellation)
            .expect("cancelled callback should stop cleanly");

        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
