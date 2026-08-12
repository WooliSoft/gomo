use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        is_raw_mode_enabled,
    },
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Wrap},
};

use crate::cancellation::CancellationToken;
use crate::commands::run::{TaskCacheStatus, TaskOutcome, TaskStatus, TaskSummary};
use crate::runner::Target;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const LONG_LIVED_SPINNER_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(crate) struct RunManyControl {
    input: Arc<Mutex<mpsc::Receiver<KeyEvent>>>,
    continuous: bool,
    cancellation: CancellationToken,
    idle_terminal: Weak<Mutex<Option<RunManyTerminal>>>,
}

impl RunManyControl {
    pub(crate) fn take_idle_terminal(&self) -> io::Result<Option<RunManyTerminal>> {
        let Some(idle_terminal) = self.idle_terminal.upgrade() else {
            return Ok(None);
        };
        let mut idle_terminal = idle_terminal
            .lock()
            .map_err(|_| io::Error::other("run TUI state lock was poisoned"))?;
        Ok(idle_terminal.take())
    }

    pub(crate) fn store_idle_terminal(&self, terminal: RunManyTerminal) -> io::Result<()> {
        let Some(idle_terminal) = self.idle_terminal.upgrade() else {
            return Ok(());
        };
        let mut idle_terminal = idle_terminal
            .lock()
            .map_err(|_| io::Error::other("run TUI state lock was poisoned"))?;
        *idle_terminal = Some(terminal);
        Ok(())
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn show_watch_error(&self, message: impl Into<String>) -> io::Result<()> {
        let Some(idle_terminal) = self.idle_terminal.upgrade() else {
            return Ok(());
        };
        let mut idle_terminal = idle_terminal
            .lock()
            .map_err(|_| io::Error::other("run TUI state lock was poisoned"))?;
        if let Some(terminal) = idle_terminal.as_mut() {
            terminal.show_watch_error(message.into())?;
        }
        Ok(())
    }

    pub(crate) fn show_waiting(
        &self,
        project_names: &[String],
        target: Target,
        parallelism: usize,
    ) -> io::Result<()> {
        let Some(idle_terminal) = self.idle_terminal.upgrade() else {
            return Ok(());
        };
        let mut idle_terminal = idle_terminal
            .lock()
            .map_err(|_| io::Error::other("run TUI state lock was poisoned"))?;
        if let Some(terminal) = idle_terminal.as_mut() {
            return terminal.show_waiting();
        }
        let mut terminal = RunManyTerminal::new_with_control(
            project_names,
            target,
            BTreeMap::new(),
            parallelism,
            Some(self.clone()),
        );
        terminal.set_waiting();
        terminal.start()?;
        *idle_terminal = Some(terminal);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct RunManySession {
    input: mpsc::Sender<KeyEvent>,
    cancellation: CancellationToken,
    idle_terminal: Arc<Mutex<Option<RunManyTerminal>>>,
}

impl RunManySession {
    pub(crate) fn continuous() -> (Self, RunManyControl) {
        let (input, receiver) = mpsc::channel();
        let idle_terminal = Arc::new(Mutex::new(None));
        let cancellation = CancellationToken::new();
        let control = RunManyControl {
            input: Arc::new(Mutex::new(receiver)),
            continuous: true,
            cancellation: cancellation.clone(),
            idle_terminal: Arc::downgrade(&idle_terminal),
        };
        (
            Self {
                input,
                cancellation,
                idle_terminal,
            },
            control,
        )
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn handle_key(&self, code: KeyCode) -> io::Result<()> {
        self.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub(crate) fn handle_key_event(&self, key: KeyEvent) -> io::Result<()> {
        let mut idle_terminal = self
            .idle_terminal
            .lock()
            .map_err(|_| io::Error::other("run TUI state lock was poisoned"))?;
        if let Some(terminal) = idle_terminal.as_mut() {
            terminal.handle_key_event(key);
            terminal.draw()
        } else {
            let _ = self.input.send(key);
            Ok(())
        }
    }

    pub(crate) fn show_waiting(
        &self,
        project_names: &[String],
        target: Target,
        parallelism: usize,
        control: RunManyControl,
    ) -> io::Result<()> {
        control.show_waiting(project_names, target, parallelism)
    }
}

pub(crate) struct RunManyTerminal {
    target: Target,
    /// Overrides the summary header label (e.g. task name for persistent sessions).
    title_override: Option<String>,
    command_displays: BTreeMap<String, String>,
    project_names: Vec<String>,
    rows: BTreeMap<String, TaskRow>,
    logs: BTreeMap<String, String>,
    selected_index: usize,
    fullscreen_logs: bool,
    log_scroll_end: Option<usize>,
    log_line_count: usize,
    log_viewport_lines: usize,
    parallelism: usize,
    started_at: Instant,
    spinner_frame: usize,
    spinner_advanced_at: Instant,
    completed: usize,
    succeeded: usize,
    failed: usize,
    cached: usize,
    skipped: usize,
    summary: Option<TaskSummary>,
    exit_prompt: Option<ExitPrompt>,
    waiting: bool,
    shutting_down: bool,
    /// When true, `q` exits immediately (persistent task supervisor).
    immediate_quit: bool,
    watch_error: Option<String>,
    terminal: Option<TuiTerminal>,
    raw_mode_enabled: bool,
    control: Option<RunManyControl>,
}

impl RunManyTerminal {
    pub(crate) fn new_with_control(
        project_names: &[String],
        target: Target,
        command_displays: BTreeMap<String, String>,
        parallelism: usize,
        control: Option<RunManyControl>,
    ) -> Self {
        let rows = project_names
            .iter()
            .map(|project| {
                (
                    project.clone(),
                    TaskRow {
                        status: TaskRowStatus::Pending,
                        cache_status: None,
                        started_at: None,
                        duration: None,
                    },
                )
            })
            .collect();

        Self {
            target,
            title_override: None,
            command_displays,
            project_names: project_names.to_vec(),
            rows,
            logs: BTreeMap::new(),
            selected_index: 0,
            fullscreen_logs: false,
            log_scroll_end: None,
            log_line_count: 0,
            log_viewport_lines: 0,
            parallelism,
            started_at: Instant::now(),
            spinner_frame: 0,
            spinner_advanced_at: Instant::now(),
            completed: 0,
            succeeded: 0,
            failed: 0,
            cached: 0,
            skipped: 0,
            summary: None,
            exit_prompt: None,
            waiting: false,
            shutting_down: false,
            immediate_quit: false,
            watch_error: None,
            terminal: None,
            raw_mode_enabled: false,
            control,
        }
    }

    /// Persistent-task supervisor: same look as finite runs, `q` exits immediately.
    pub(crate) fn new_persistent(
        step_names: &[String],
        title: impl Into<String>,
        command_displays: BTreeMap<String, String>,
        parallelism: usize,
    ) -> Self {
        let mut terminal = Self::new_with_control(
            step_names,
            Target::Build,
            command_displays,
            parallelism,
            None,
        );
        terminal.title_override = Some(title.into());
        terminal.immediate_quit = true;
        terminal
    }

    pub(crate) fn start(&mut self) -> io::Result<()> {
        if !is_raw_mode_enabled()? {
            enable_raw_mode()?;
            self.raw_mode_enabled = true;
        }

        let mut stdout = io::stdout();
        if !self.is_continuous() {
            execute!(stdout, EnterAlternateScreen)?;
        }
        execute!(stdout, cursor::Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        self.terminal = Some(terminal);
        self.draw()
    }

    pub(crate) fn task_started(&mut self, project: &str) -> io::Result<()> {
        self.handle_input()?;
        if let Some(row) = self.rows.get_mut(project) {
            row.status = TaskRowStatus::Running;
            row.started_at = Some(Instant::now());
        }
        let command_line = self.command_log_line(project);
        let log = self.logs.entry(project.to_string()).or_default();
        if log.is_empty() {
            log.push_str(&command_line);
        }
        self.draw()
    }

    pub(crate) fn task_completed(&mut self, outcome: &TaskOutcome, output: &str) -> io::Result<()> {
        self.handle_input()?;
        self.finish_row(outcome);
        if outcome.cache_status == Some(TaskCacheStatus::Hit) {
            let command_display = self.command_display(&outcome.project);
            let cached_output = cached_log_output(output, &command_display);
            if !cached_output.trim().is_empty() {
                let log = self.logs.entry(outcome.project.clone()).or_default();
                if !log.ends_with('\n') {
                    log.push('\n');
                }
                log.push_str(cached_output.trim_start());
            }
        }
        self.draw()
    }

    pub(crate) fn task_output(&mut self, project: &str, chunk: &str) -> io::Result<()> {
        self.handle_input()?;
        self.logs
            .entry(project.to_string())
            .or_default()
            .push_str(chunk);
        self.draw()
    }

    pub(crate) fn append_log_silent(&mut self, project: &str, chunk: &str) {
        self.logs
            .entry(project.to_string())
            .or_default()
            .push_str(chunk);
    }

    pub(crate) fn poll_quit(&mut self) -> io::Result<bool> {
        while let Some(key) = self.next_key(Duration::from_millis(0))? {
            if self.handle_key_event(key) {
                self.begin_shutdown();
                self.draw()?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn restore_now(&mut self) -> io::Result<()> {
        self.restore()
    }

    pub(crate) fn task_skipped(&mut self, outcome: &TaskOutcome) -> io::Result<()> {
        self.handle_input()?;
        self.finish_row(outcome);
        self.draw()
    }

    pub(crate) fn tick(&mut self) -> io::Result<()> {
        self.handle_input()?;
        if !self.is_long_lived()
            || self.spinner_advanced_at.elapsed() >= LONG_LIVED_SPINNER_INTERVAL
        {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            self.spinner_advanced_at = Instant::now();
        }
        self.draw()
    }

    pub(crate) fn finish(&mut self, summary: &TaskSummary) -> io::Result<RunManyExit> {
        self.summary = Some(summary.clone());
        self.draw()?;
        if self.is_continuous() {
            return Ok(RunManyExit::Continued);
        }
        if super::is_agent_environment() {
            self.restore()?;
            return Ok(RunManyExit::AutoExited);
        }
        self.exit_prompt = Some(ExitPrompt::Cancelled);
        self.draw()?;
        let exit = self.wait_for_exit_confirmation()?;
        self.restore()?;
        Ok(exit)
    }

    pub(crate) fn abort(&mut self) -> io::Result<()> {
        self.restore()
    }

    pub(crate) fn show_waiting(&mut self) -> io::Result<()> {
        self.set_waiting();
        self.draw()
    }

    fn show_watch_error(&mut self, message: String) -> io::Result<()> {
        self.watch_error = Some(message);
        self.draw()
    }

    fn set_waiting(&mut self) {
        self.waiting = true;
        self.watch_error = None;
        for row in self.rows.values_mut() {
            row.status = TaskRowStatus::Waiting;
        }
    }

    fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        for row in self.rows.values_mut() {
            if row.status == TaskRowStatus::Running {
                row.status = TaskRowStatus::Stopping;
            }
        }
    }

    fn finish_row(&mut self, outcome: &TaskOutcome) {
        let duration = self
            .rows
            .get(&outcome.project)
            .and_then(|row| row.started_at)
            .map(|started_at| started_at.elapsed())
            .unwrap_or_default();

        if let Some(row) = self.rows.get_mut(&outcome.project) {
            row.duration = Some(duration);
            row.cache_status = outcome.cache_status;
            if self.shutting_down {
                return;
            }
            row.status = match outcome.status {
                TaskStatus::Succeeded if outcome.cache_status == Some(TaskCacheStatus::Hit) => {
                    TaskRowStatus::Cached
                }
                TaskStatus::Succeeded => TaskRowStatus::Succeeded,
                TaskStatus::Failed(exit_code) => TaskRowStatus::Failed(exit_code),
                TaskStatus::Skipped => TaskRowStatus::Skipped,
            };
        }

        self.completed += 1;
        match outcome.status {
            TaskStatus::Succeeded => {
                self.succeeded += 1;
                if outcome.cache_status == Some(TaskCacheStatus::Hit) {
                    self.cached += 1;
                }
            }
            TaskStatus::Failed(_) => self.failed += 1,
            TaskStatus::Skipped => self.skipped += 1,
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        let Some(mut terminal) = self.terminal.take() else {
            return Ok(());
        };

        terminal.draw(|frame| render(frame, self))?;
        self.terminal = Some(terminal);
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        if let Some(mut terminal) = self.terminal.take()
            && !self.is_continuous()
        {
            execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
            terminal.show_cursor()?;
        }
        if self.raw_mode_enabled {
            disable_raw_mode()?;
            self.raw_mode_enabled = false;
        }
        io::stdout().flush()
    }

    fn is_continuous(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.continuous)
    }

    fn shows_elapsed(&self) -> bool {
        !self.is_long_lived()
    }

    fn is_long_lived(&self) -> bool {
        self.immediate_quit || self.is_continuous()
    }

    fn wait_for_exit_confirmation(&mut self) -> io::Result<RunManyExit> {
        loop {
            if let Some(key) = self.next_key(Duration::from_millis(250))? {
                if self.handle_key_event(key) {
                    return Ok(RunManyExit::UserExited);
                }
                self.draw()?;
            }
        }
    }

    fn handle_input(&mut self) -> io::Result<()> {
        if self.terminal.is_none() {
            return Ok(());
        }

        while let Some(key) = self.next_key(Duration::from_millis(0))? {
            self.handle_key_event(key);
        }

        Ok(())
    }

    fn next_key(&self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
        let Some(control) = self.control.as_ref() else {
            if !event::poll(timeout)? {
                return Ok(None);
            }
            return Ok(match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
                _ => None,
            });
        };
        let receiver = control
            .input
            .lock()
            .map_err(|_| io::Error::other("run TUI input channel lock was poisoned"))?;
        match receiver.recv_timeout(timeout) {
            Ok(key) => Ok(Some(key)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "run TUI input closed",
            )),
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if self.immediate_quit && is_persistent_quit_key(key) {
            return true;
        }
        match key.code {
            KeyCode::Enter
                if self.summary.is_some() && self.exit_prompt == Some(ExitPrompt::Cancelled) =>
            {
                self.fullscreen_logs = true;
            }
            KeyCode::Enter if self.summary.is_some() => return true,
            KeyCode::Char('q') if self.summary.is_some() => return true,
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.fullscreen_logs = !self.fullscreen_logs;
                self.log_scroll_end = None;
            }
            KeyCode::Esc if self.fullscreen_logs => {
                self.fullscreen_logs = false;
                self.log_scroll_end = None;
            }
            KeyCode::Esc if self.summary.is_some() => return true,
            KeyCode::Char('k') if self.fullscreen_logs => {
                let minimum_end = self.log_viewport_lines.min(self.log_line_count);
                let current_end = self.log_scroll_end.unwrap_or(self.log_line_count);
                let next_end = current_end.saturating_sub(1).max(minimum_end);
                self.log_scroll_end = (next_end < self.log_line_count).then_some(next_end);
            }
            KeyCode::Char('j') if self.fullscreen_logs => {
                if let Some(current_end) = self.log_scroll_end {
                    let next_end = current_end.saturating_add(1);
                    self.log_scroll_end = (next_end < self.log_line_count).then_some(next_end);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_index = self.selected_index.saturating_sub(1);
                self.log_scroll_end = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.project_names.is_empty() {
                    self.selected_index =
                        (self.selected_index + 1).min(self.project_names.len() - 1);
                    self.log_scroll_end = None;
                }
            }
            KeyCode::Home => {
                self.selected_index = 0;
                self.log_scroll_end = None;
            }
            KeyCode::End if !self.project_names.is_empty() => {
                self.selected_index = self.project_names.len() - 1;
                self.log_scroll_end = None;
            }
            _ => {}
        }

        false
    }

    fn selected_project(&self) -> Option<&str> {
        self.project_names
            .get(self.selected_index)
            .map(String::as_str)
    }

    fn command_display(&self, project: &str) -> String {
        self.command_displays
            .get(project)
            .cloned()
            .unwrap_or_else(|| self.target.command_display())
    }

    fn command_log_line(&self, project: &str) -> String {
        format!("$ {}\n", self.command_display(project))
    }
}

impl Drop for RunManyTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug, Clone)]
struct TaskRow {
    status: TaskRowStatus,
    cache_status: Option<TaskCacheStatus>,
    started_at: Option<Instant>,
    duration: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskRowStatus {
    Waiting,
    Pending,
    Running,
    Stopping,
    Succeeded,
    Cached,
    Failed(i32),
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitPrompt {
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunManyExit {
    AutoExited,
    Continued,
    UserExited,
}

pub(crate) fn render_terminal_summary(summary: &TaskSummary) -> String {
    let mut output = String::new();
    if summary.failed == 0 {
        output.push_str(&format!(
            "Gomo successfully ran {}.\n",
            format_target_and_projects(summary.target, summary.total)
        ));
        if summary.cache_hits > 0 {
            output.push_str(&format!(
                "Gomo read the output from cache for {} out of {} tasks.\n",
                summary.cache_hits, summary.total
            ));
        }
    } else {
        output.push_str(&format!(
            "Gomo ran {} with {} failed task(s).\n",
            format_target_and_projects(summary.target, summary.total),
            summary.failed
        ));
        for outcome in summary
            .outcomes
            .iter()
            .filter(|outcome| matches!(outcome.status, TaskStatus::Failed(_)))
            .take(5)
        {
            output.push_str(&format!(
                "- {}\n",
                task_id(&outcome.project, summary.target)
            ));
        }
    }
    output
}

fn render(frame: &mut Frame<'_>, app: &mut RunManyTerminal) {
    if app.fullscreen_logs {
        render_fullscreen_logs(frame, frame.area(), app);
        return;
    }

    let summary_height = if app.shows_elapsed() { 8 } else { 7 };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(summary_height)])
        .split(frame.area());

    render_body(frame, outer[0], app);
    render_run_summary(frame, outer[1], app);
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &mut RunManyTerminal) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(area);

    render_task_table(frame, chunks[0], app);
    render_log_panel(frame, chunks[1], app);
}

fn render_task_table(frame: &mut Frame<'_>, area: Rect, app: &RunManyTerminal) {
    let header = Row::new(["Status", "Task", "Cache", "Time"])
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let rows = app
        .project_names
        .iter()
        .enumerate()
        .map(|(index, project)| {
            let row = app.rows.get(project).expect("project row exists");
            let selected = index == app.selected_index;
            let style = if selected {
                Style::new().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::new()
            };
            Row::new([
                Cell::from(status_line(row.status, app.spinner_frame)),
                Cell::from(task_id(project, app.target)),
                Cell::from(cache_label(row.cache_status)),
                Cell::from(
                    row.duration
                        .map(pretty_duration)
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ])
            .style(style)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Min(18),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::bordered()
            .title(" Tasks ")
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray)),
    )
    .column_spacing(2);
    frame.render_widget(table, area);
}

fn render_log_panel(frame: &mut Frame<'_>, area: Rect, app: &mut RunManyTerminal) {
    let lines = log_lines(app, area.height.saturating_sub(2) as usize);
    let panel = Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(" Logs ")
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

fn render_fullscreen_logs(frame: &mut Frame<'_>, area: Rect, app: &mut RunManyTerminal) {
    let selected = app.selected_project().unwrap_or("-");
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let header = Paragraph::new(log_title(app, selected)).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray)),
    );
    frame.render_widget(header, chunks[0]);

    let lines = log_lines(app, chunks[1].height.saturating_sub(1) as usize);
    let logs = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::new().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(logs, chunks[1]);
}

fn log_title(app: &RunManyTerminal, project: &str) -> Line<'static> {
    let mut title = vec![
        raw("Logs: "),
        styled(task_id(project, app.target), Color::White, Modifier::BOLD),
    ];
    append_cache_title(app, project, &mut title);
    title.push(raw("  "));
    title.push(dim(key_hint(app)));
    Line::from(title)
}

fn log_lines(app: &mut RunManyTerminal, max_lines: usize) -> Vec<Line<'static>> {
    let selected = app.selected_project().unwrap_or("-");
    let output = app
        .logs
        .get(selected)
        .map(String::as_str)
        .filter(|log| !log.trim().is_empty())
        .unwrap_or_else(|| {
            if app
                .rows
                .get(selected)
                .is_some_and(|row| row.status == TaskRowStatus::Running)
            {
                "Task is running. Waiting for log output..."
            } else {
                "No log output captured for this task."
            }
        });
    let output = filter_log_metadata(output);
    let max_lines = max_lines.max(1);
    let requested_end = app.log_scroll_end;
    let (lines, line_count, end) = ansi_log_lines(&output, max_lines, requested_end);
    app.log_line_count = line_count;
    app.log_viewport_lines = max_lines;
    app.log_scroll_end = requested_end.and_then(|_| (end < line_count).then_some(end));
    lines
}

fn cached_log_output(output: &str, command_display: &str) -> String {
    output
        .lines()
        .filter(|line| !is_hidden_cached_output_line(line, command_display))
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_cache_title(app: &RunManyTerminal, project: &str, title: &mut Vec<Span<'static>>) {
    if app
        .rows
        .get(project)
        .is_some_and(|row| row.cache_status == Some(TaskCacheStatus::Hit))
    {
        title.push(raw("  "));
        title.push(styled("Cache", Color::Green, Modifier::BOLD));
    }
}

fn filter_log_metadata(input: &str) -> String {
    input
        .lines()
        .filter(|line| !is_hidden_log_metadata(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_hidden_log_metadata(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("[cache hit]")
        || trimmed.starts_with("[cache hit local]")
        || trimmed.starts_with("[cache hit remote")
}

fn is_hidden_cached_output_line(line: &str, command_display: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("==>")
        || trimmed == format!("$ {command_display}")
        || is_hidden_log_metadata(line)
}

fn ansi_log_lines(
    input: &str,
    max_lines: usize,
    requested_end: Option<usize>,
) -> (Vec<Line<'static>>, usize, usize) {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = Style::new();
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '\x1b' => handle_ansi_escape(&mut chars, &mut spans, &mut text, &mut style),
            '\u{9b}' => handle_csi_sequence(&mut chars, &mut spans, &mut text, &mut style),
            '\n' => push_log_line(&mut lines, &mut spans, &mut text, style),
            '\r' => push_log_line(&mut lines, &mut spans, &mut text, style),
            _ => text.push(character),
        }
    }

    if !text.is_empty() || !spans.is_empty() {
        push_log_line(&mut lines, &mut spans, &mut text, style);
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    let line_count = lines.len();
    let minimum_end = max_lines.min(line_count);
    let end = requested_end
        .unwrap_or(line_count)
        .clamp(minimum_end, line_count);
    let start = end.saturating_sub(max_lines);
    (lines.drain(start..end).collect(), line_count, end)
}

fn handle_ansi_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    style: &mut Style,
) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            handle_csi_sequence(chars, spans, text, style);
        }
        Some(']') => {
            chars.next();
            strip_osc_sequence(chars);
        }
        Some('(' | ')' | '*' | '+' | '-' | '.' | '/') => {
            chars.next();
            chars.next();
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn handle_csi_sequence(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    style: &mut Style,
) {
    let mut parameters = String::new();
    for character in chars.by_ref() {
        if ('@'..='~').contains(&character) {
            if character == 'm' {
                push_log_span(spans, text, *style);
                apply_sgr_parameters(&parameters, style);
            }
            break;
        }
        parameters.push(character);
    }
}

fn strip_osc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = chars.next() {
        if character == '\u{7}' {
            break;
        }
        if character == '\x1b' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

fn push_log_line(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    style: Style,
) {
    push_log_span(spans, text, style);
    lines.push(Line::from(std::mem::take(spans)));
}

fn push_log_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if text.is_empty() {
        return;
    }

    spans.push(Span::styled(std::mem::take(text), style));
}

fn apply_sgr_parameters(parameters: &str, style: &mut Style) {
    let mut codes = if parameters.is_empty() {
        vec![0]
    } else {
        parameters
            .split([';', ':'])
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    if codes.is_empty() {
        codes.push(0);
    }

    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => *style = Style::new(),
            1 => *style = (*style).add_modifier(Modifier::BOLD),
            2 => *style = (*style).add_modifier(Modifier::DIM),
            3 => *style = (*style).add_modifier(Modifier::ITALIC),
            4 => *style = (*style).add_modifier(Modifier::UNDERLINED),
            7 => *style = (*style).add_modifier(Modifier::REVERSED),
            9 => *style = (*style).add_modifier(Modifier::CROSSED_OUT),
            22 => *style = (*style).remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = (*style).remove_modifier(Modifier::ITALIC),
            24 => *style = (*style).remove_modifier(Modifier::UNDERLINED),
            27 => *style = (*style).remove_modifier(Modifier::REVERSED),
            29 => *style = (*style).remove_modifier(Modifier::CROSSED_OUT),
            30..=37 | 90..=97 => {
                if let Some(color) = ansi_color(codes[index], false) {
                    *style = (*style).fg(color);
                }
            }
            39 => *style = (*style).fg(Color::Reset),
            40..=47 | 100..=107 => {
                if let Some(color) = ansi_color(codes[index], true) {
                    *style = (*style).bg(color);
                }
            }
            49 => *style = (*style).bg(Color::Reset),
            38 | 48 => {
                if let Some((color, consumed)) = extended_ansi_color(&codes[index + 1..]) {
                    if codes[index] == 38 {
                        *style = (*style).fg(color);
                    } else {
                        *style = (*style).bg(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn ansi_color(code: u16, background: bool) -> Option<Color> {
    let code = if background {
        code.saturating_sub(10)
    } else {
        code
    };
    Some(match code {
        30 => Color::Black,
        31 => Color::Red,
        32 => Color::Green,
        33 => Color::Yellow,
        34 => Color::Blue,
        35 => Color::Magenta,
        36 => Color::Cyan,
        37 => Color::Gray,
        90 => Color::DarkGray,
        91 => Color::LightRed,
        92 => Color::LightGreen,
        93 => Color::LightYellow,
        94 => Color::LightBlue,
        95 => Color::LightMagenta,
        96 => Color::LightCyan,
        97 => Color::White,
        _ => return None,
    })
}

fn extended_ansi_color(codes: &[u16]) -> Option<(Color, usize)> {
    match codes {
        [5, value, ..] => Some((Color::Indexed((*value).min(u8::MAX as u16) as u8), 2)),
        [2, red, green, blue, ..] => Some((
            Color::Rgb(
                (*red).min(u8::MAX as u16) as u8,
                (*green).min(u8::MAX as u16) as u8,
                (*blue).min(u8::MAX as u16) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

fn render_run_summary(frame: &mut Frame<'_>, area: Rect, app: &RunManyTerminal) {
    let has_live_failure = app
        .rows
        .values()
        .any(|row| matches!(row.status, TaskRowStatus::Failed(_)));
    let status_color = if app.watch_error.is_some()
        || if app.is_long_lived() {
            has_live_failure
        } else {
            app.failed > 0
        } {
        Color::Red
    } else if app.summary.is_some() {
        Color::Green
    } else {
        Color::Cyan
    };
    let mut lines = if app.is_long_lived() {
        long_lived_summary_lines(app, status_color)
    } else {
        finite_summary_lines(app, status_color)
    };
    lines.push(Line::from(vec![
        dim("Keys"),
        raw("      "),
        raw(key_hint(app)),
    ]));
    let summary = Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(" Summary ")
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(summary, area);
}

fn long_lived_summary_header(
    app: &RunManyTerminal,
    status_color: Color,
    item_label: &str,
) -> Line<'static> {
    Line::from(vec![
        styled("Gomo", status_color, Modifier::BOLD),
        raw(" "),
        styled(
            app.title_override
                .clone()
                .unwrap_or_else(|| app.target.as_str().to_string()),
            Color::White,
            Modifier::BOLD,
        ),
        dim(format!(
            "  {} {item_label}  parallel {}",
            app.project_names.len(),
            app.parallelism
        )),
    ])
}

fn watch_notice(app: &RunManyTerminal) -> Line<'static> {
    app.watch_error.as_ref().map_or_else(
        || Line::from(""),
        |message| {
            Line::from(vec![
                dim("Error"),
                raw("     "),
                styled(message.clone(), Color::Red, Modifier::BOLD),
            ])
        },
    )
}

fn long_lived_summary_lines(app: &RunManyTerminal, status_color: Color) -> Vec<Line<'static>> {
    let count = |status| app.rows.values().filter(|row| row.status == status).count();
    let waiting = count(TaskRowStatus::Waiting);
    let queued = count(TaskRowStatus::Pending);
    let running = count(TaskRowStatus::Running);
    let stopping = count(TaskRowStatus::Stopping);
    let finished = count(TaskRowStatus::Succeeded)
        + count(TaskRowStatus::Cached)
        + count(TaskRowStatus::Skipped);
    let failed = app
        .rows
        .values()
        .filter(|row| matches!(row.status, TaskRowStatus::Failed(_)))
        .count();
    let status = if app.waiting || waiting > 0 {
        "waiting for changes"
    } else if stopping > 0 || app.shutting_down {
        "stopping"
    } else if running > 0 {
        if failed > 0 {
            "running with failures"
        } else {
            "running"
        }
    } else if failed > 0 {
        if app.is_continuous() {
            "cycle failed"
        } else {
            "failed"
        }
    } else if queued > 0 {
        if finished > 0 { "running" } else { "starting" }
    } else if app.is_continuous() {
        "cycle complete"
    } else if finished > 0 {
        "stopped"
    } else {
        "starting"
    };
    let (item_label, item_count_label) = if app.is_continuous() {
        ("Projects", "projects")
    } else {
        ("Services", "services")
    };

    let mut lines = vec![
        long_lived_summary_header(app, status_color, item_count_label),
        watch_notice(app),
        Line::from(vec![
            dim("Status"),
            raw("    "),
            styled(status, status_color, Modifier::BOLD),
        ]),
    ];
    lines.push(if app.is_continuous() {
        Line::from(vec![
            dim(item_label),
            raw("  "),
            styled(format!("watching {waiting}"), Color::Cyan, Modifier::BOLD),
            raw("   "),
            styled(format!("running {running}"), Color::Green, Modifier::BOLD),
            raw("   "),
            dim(format!("queued {queued}")),
            raw("   "),
            dim(format!("done {finished}")),
            raw("   "),
            styled(format!("failed {failed}"), Color::Red, Modifier::BOLD),
        ])
    } else {
        Line::from(vec![
            dim(item_label),
            raw("  "),
            styled(format!("running {running}"), Color::Green, Modifier::BOLD),
            raw("   "),
            dim(format!("queued {queued}")),
            raw("   "),
            dim(format!("stopping {stopping}")),
            raw("   "),
            dim(format!("stopped {finished}")),
            raw("   "),
            styled(format!("failed {failed}"), Color::Red, Modifier::BOLD),
        ])
    });
    lines
}

fn finite_summary_lines(app: &RunManyTerminal, status_color: Color) -> Vec<Line<'static>> {
    let running = app
        .rows
        .values()
        .filter(|row| row.status == TaskRowStatus::Running)
        .count();
    let remaining = app.project_names.len().saturating_sub(app.completed);
    let percentage = if app.project_names.is_empty() {
        100
    } else {
        app.completed * 100 / app.project_names.len()
    };
    let progress = Line::from(vec![
        dim("Progress"),
        raw("  "),
        styled(
            format!("{} / {} complete", app.completed, app.project_names.len()),
            status_color,
            Modifier::BOLD,
        ),
        dim(format!("  {percentage}%")),
    ]);
    let mut lines = vec![
        Line::from(vec![
            styled("Gomo", status_color, Modifier::BOLD),
            raw(" "),
            styled(
                app.title_override
                    .clone()
                    .unwrap_or_else(|| app.target.as_str().to_string()),
                Color::White,
                Modifier::BOLD,
            ),
            dim(format!(
                "  {} total  {} running  {} remaining  parallel {}",
                app.project_names.len(),
                running,
                remaining,
                app.parallelism
            )),
        ]),
        watch_notice(app),
        progress,
        Line::from(vec![
            dim("Results"),
            raw("   "),
            styled(
                format!("ok {}", app.succeeded),
                Color::Green,
                Modifier::BOLD,
            ),
            raw("   "),
            styled(
                format!("cached {}", app.cached),
                Color::Cyan,
                Modifier::BOLD,
            ),
            raw("   "),
            styled(format!("failed {}", app.failed), Color::Red, Modifier::BOLD),
            raw("   "),
            dim(format!("skipped {}", app.skipped)),
        ]),
    ];
    if app.shows_elapsed() {
        lines.push(Line::from(vec![
            dim("Elapsed"),
            raw("   "),
            raw(pretty_duration(app.started_at.elapsed())),
        ]));
    }
    lines
}

fn is_persistent_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q' | 'Q')) && key.modifiers.is_empty()
        || matches!(key.code, KeyCode::Char('c')) && key.modifiers == KeyModifiers::CONTROL
}

fn key_hint(app: &RunManyTerminal) -> &'static str {
    if app.fullscreen_logs && app.immediate_quit {
        "j/k scroll, ↑/↓ select, L/Esc back, q or Ctrl-C exit"
    } else if app.fullscreen_logs {
        "j/k scroll, ↑/↓ select, L/Esc back"
    } else if app.immediate_quit {
        "↑/↓ or j/k select, L logs, q or Ctrl-C exit"
    } else if app
        .control
        .as_ref()
        .is_some_and(|control| control.continuous)
    {
        "↑/↓ or j/k select, Enter/L logs, q exits watch"
    } else if app.summary.is_some() {
        "↑/↓ tasks, Enter/L logs, Esc/q exits"
    } else {
        "↑/↓ or j/k select tasks, L logs"
    }
}

fn status_line(status: TaskRowStatus, spinner_frame: usize) -> Line<'static> {
    Line::from(status_spans(status, spinner_frame))
}

fn status_spans(status: TaskRowStatus, spinner_frame: usize) -> Vec<Span<'static>> {
    match status {
        TaskRowStatus::Waiting => vec![dim("watching")],
        TaskRowStatus::Pending => vec![dim("queued")],
        TaskRowStatus::Running => vec![
            styled(
                SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
                Color::Cyan,
                Modifier::BOLD,
            ),
            raw(" running"),
        ],
        TaskRowStatus::Stopping => vec![dim("stopping")],
        TaskRowStatus::Succeeded => vec![styled("✓", Color::Green, Modifier::BOLD), raw(" ok")],
        TaskRowStatus::Cached => vec![styled("✓", Color::Green, Modifier::BOLD), raw(" cached")],
        TaskRowStatus::Failed(_) => vec![styled("✗", Color::Red, Modifier::BOLD), raw(" failed")],
        TaskRowStatus::Skipped => vec![dim("skipped")],
    }
}

fn cache_label(cache_status: Option<TaskCacheStatus>) -> String {
    match cache_status {
        Some(TaskCacheStatus::Hit) => "hit".to_string(),
        Some(TaskCacheStatus::Miss) => "miss".to_string(),
        Some(TaskCacheStatus::Bypassed) => "bypass".to_string(),
        None => "-".to_string(),
    }
}

fn format_target_and_projects(target: Target, total: usize) -> String {
    if total == 1 {
        format!("target {target} for 1 project")
    } else {
        format!("target {target} for {total} projects")
    }
}

fn task_id(project: &str, target: Target) -> String {
    format!("{project}:{target}")
}

fn pretty_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        return format!("{millis}ms");
    }

    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }

    let minutes = duration.as_secs() / 60;
    let seconds = duration.as_secs() % 60;
    format!("{minutes}m {seconds}s")
}

fn raw(text: impl Into<String>) -> Span<'static> {
    Span::raw(text.into())
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), dim_style())
}

fn styled(text: impl Into<String>, color: Color, modifier: Modifier) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color).add_modifier(modifier))
}

fn dim_style() -> Style {
    Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_target_and_project_count() {
        assert_eq!(
            format_target_and_projects(Target::Build, 1),
            "target build for 1 project"
        );
        assert_eq!(
            format_target_and_projects(Target::Test, 3),
            "target test for 3 projects"
        );
    }

    #[test]
    fn formats_short_and_long_durations() {
        assert_eq!(pretty_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(pretty_duration(Duration::from_millis(1250)), "1.2s");
        assert_eq!(pretty_duration(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn hides_elapsed_for_long_lived_sessions() {
        let finite = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            None,
        );
        let persistent =
            RunManyTerminal::new_persistent(&["one".to_string()], "dev", BTreeMap::new(), 1);
        let (_session, control) = RunManySession::continuous();
        let watch = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control),
        );

        assert!(finite.shows_elapsed());
        assert!(!persistent.shows_elapsed());
        assert!(!watch.shows_elapsed());
    }

    #[test]
    fn long_lived_summaries_show_lifecycle_instead_of_completion() {
        let mut persistent = RunManyTerminal::new_persistent(
            &["api".to_string(), "web".to_string()],
            "dev",
            BTreeMap::new(),
            2,
        );
        persistent.rows.get_mut("api").unwrap().status = TaskRowStatus::Running;
        let persistent_text = line_text(&long_lived_summary_lines(&persistent, Color::Cyan));

        assert!(persistent_text.contains("2 services"));
        assert!(persistent_text.contains("Status    running"));
        assert!(persistent_text.contains("Services  running 1   queued 1"));
        assert!(!persistent_text.contains("Progress"));
        assert!(!persistent_text.contains("Results"));

        let (_session, control) = RunManySession::continuous();
        let mut watch = RunManyTerminal::new_with_control(
            &["one".to_string(), "two".to_string()],
            Target::Build,
            BTreeMap::new(),
            2,
            Some(control),
        );
        watch.set_waiting();
        let watch_text = line_text(&long_lived_summary_lines(&watch, Color::Cyan));

        assert!(watch_text.contains("2 projects"));
        assert!(watch_text.contains("Status    waiting for changes"));
        assert!(watch_text.contains("Projects  watching 2"));
        assert!(!watch_text.contains("complete"));

        watch.waiting = false;
        watch.rows.get_mut("one").unwrap().status = TaskRowStatus::Succeeded;
        watch.rows.get_mut("two").unwrap().status = TaskRowStatus::Pending;
        let watch_text = line_text(&long_lived_summary_lines(&watch, Color::Cyan));
        assert!(watch_text.contains("Status    running"));
        assert!(watch_text.contains("queued 1   done 1"));

        persistent.rows.get_mut("api").unwrap().status = TaskRowStatus::Succeeded;
        persistent.rows.get_mut("web").unwrap().status = TaskRowStatus::Succeeded;
        let persistent_text = line_text(&long_lived_summary_lines(&persistent, Color::Cyan));
        assert!(persistent_text.contains("Status    stopped"));
        assert!(persistent_text.contains("stopped 2"));
    }

    #[test]
    fn finite_summary_keeps_completion_metrics() {
        let finite = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            None,
        );
        let text = line_text(&finite_summary_lines(&finite, Color::Cyan));

        assert!(text.contains("Progress"));
        assert!(text.contains("Results"));
        assert!(text.contains("Elapsed"));
    }

    fn line_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn slows_spinner_for_long_lived_sessions() {
        let mut finite = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            None,
        );
        let mut persistent =
            RunManyTerminal::new_persistent(&["one".to_string()], "dev", BTreeMap::new(), 1);

        finite.tick().expect("finite spinner should tick");
        persistent.tick().expect("persistent spinner should tick");
        assert_eq!(finite.spinner_frame, 1);
        assert_eq!(persistent.spinner_frame, 0);

        persistent.spinner_advanced_at = Instant::now() - LONG_LIVED_SPINNER_INTERVAL;
        persistent.tick().expect("persistent spinner should tick");
        assert_eq!(persistent.spinner_frame, 1);
    }

    #[test]
    fn keeps_persistent_rows_stopping_during_shutdown() {
        let mut terminal =
            RunManyTerminal::new_persistent(&["dev".to_string()], "stack", BTreeMap::new(), 1);
        terminal.rows.get_mut("dev").unwrap().status = TaskRowStatus::Running;
        terminal.begin_shutdown();

        terminal.finish_row(&TaskOutcome {
            project: "dev".to_string(),
            status: TaskStatus::Succeeded,
            cache_status: None,
            cache_source: None,
            remote_scope: None,
            bytes_downloaded: 0,
            bytes_uploaded: 0,
            upload_result: None,
        });

        assert_eq!(terminal.rows["dev"].status, TaskRowStatus::Stopping);
        assert_eq!(terminal.succeeded, 0);
    }

    #[test]
    fn renders_ansi_colored_logs_as_styled_spans() {
        let (lines, _, _) = ansi_log_lines("\x1b[32m.\x1b[39m 132 passed\n", 10, None);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, ".");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].spans[1].content, " 132 passed");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Reset));

        let (lines, _, _) = ansi_log_lines("\x1b]0;title\x07ok", 10, None);
        assert_eq!(lines[0].spans[0].content, "ok");
    }

    #[test]
    fn scrolls_fullscreen_logs_without_changing_tasks() {
        let mut terminal = RunManyTerminal::new_persistent(
            &["one".to_string(), "two".to_string()],
            "stack",
            BTreeMap::new(),
            2,
        );
        terminal.fullscreen_logs = true;
        terminal.log_line_count = 10;
        terminal.log_viewport_lines = 4;

        terminal.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        terminal.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(terminal.selected_index, 0);
        assert_eq!(terminal.log_scroll_end, Some(8));

        terminal.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(terminal.selected_index, 0);
        assert_eq!(terminal.log_scroll_end, Some(9));

        terminal.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(terminal.selected_index, 1);
        assert_eq!(terminal.log_scroll_end, None);
    }

    #[test]
    fn renders_scrolled_log_window() {
        let (lines, line_count, end) = ansi_log_lines("one\ntwo\nthree\nfour\n", 2, Some(3));
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(text, ["two", "three"]);
        assert_eq!(line_count, 4);
        assert_eq!(end, 3);

        let (lines, line_count, end) = ansi_log_lines("one\ntwo\n", 2, Some(usize::MAX));
        assert_eq!(lines.len(), 2);
        assert_eq!(line_count, 2);
        assert_eq!(end, 2);
    }

    #[test]
    fn keeps_scrolled_log_window_stable_as_logs_grow() {
        let (before, _, end) = ansi_log_lines("one\ntwo\nthree\nfour\n", 2, Some(3));
        let (after, _, _) = ansi_log_lines("one\ntwo\nthree\nfour\nfive\n", 2, Some(end));

        let visible = |lines: Vec<Line<'static>>| {
            lines
                .into_iter()
                .flat_map(|line| line.spans)
                .map(|span| span.content.into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(before), ["two", "three"]);
        assert_eq!(visible(after), ["two", "three"]);
    }

    #[test]
    fn hides_cache_hit_metadata_from_logs() {
        assert_eq!(
            filter_log_metadata("[cache hit local] abc123\nreal output\n"),
            "real output"
        );
    }

    #[test]
    fn cached_log_output_keeps_only_replayed_command_output() {
        assert_eq!(
            cached_log_output(
                "==> web_app:build (apps/web_app)\n$ gleam build\n[cache hit local] abc123\ncompiled\n",
                "gleam build",
            ),
            "compiled"
        );
    }

    #[test]
    fn renders_terminal_summary_with_failed_task() {
        let summary = TaskSummary {
            target: Target::Build,
            total: 2,
            succeeded: 1,
            failed: 1,
            skipped: 0,
            cache_hits: 0,
            cache_misses: 2,
            cache_bypassed: 0,
            outcomes: vec![TaskOutcome {
                project: "web_app".to_string(),
                status: TaskStatus::Failed(1),
                cache_status: Some(TaskCacheStatus::Miss),
                cache_source: None,
                remote_scope: None,
                bytes_downloaded: 0,
                bytes_uploaded: 0,
                upload_result: None,
            }],
        };
        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("with 1 failed task"));
        assert!(rendered.contains("web_app:build"));
    }

    #[test]
    fn channel_backed_input_delivers_navigation_keys() {
        let (session, control) = RunManySession::continuous();
        let terminal = RunManyTerminal::new_with_control(
            &["one".to_string(), "two".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control),
        );
        session
            .handle_key(KeyCode::Down)
            .expect("input should be sent");

        assert_eq!(
            terminal
                .next_key(Duration::from_millis(0))
                .expect("input should be read")
                .map(|key| key.code),
            Some(KeyCode::Down)
        );
    }

    #[test]
    fn disconnected_channel_backed_input_returns_an_error() {
        let (session, control) = RunManySession::continuous();
        let terminal = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control),
        );
        drop(session);

        let error = terminal
            .next_key(Duration::from_millis(0))
            .expect_err("closed input should not look like a timeout");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn continuous_tui_returns_to_its_parent_without_waiting() {
        let (_session, control) = RunManySession::continuous();
        let mut terminal = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control),
        );
        let summary = TaskSummary {
            target: Target::Build,
            total: 1,
            succeeded: 1,
            failed: 0,
            skipped: 0,
            cache_hits: 0,
            cache_misses: 1,
            cache_bypassed: 0,
            outcomes: Vec::new(),
        };

        assert_eq!(
            terminal.finish(&summary).expect("TUI should finish"),
            RunManyExit::Continued
        );
    }

    #[test]
    fn continuous_session_keeps_handling_input_while_idle() {
        let (session, control) = RunManySession::continuous();
        let terminal = RunManyTerminal::new_with_control(
            &["one".to_string(), "two".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control.clone()),
        );
        control
            .store_idle_terminal(terminal)
            .expect("terminal should be stored");

        session
            .handle_key(KeyCode::Down)
            .expect("idle input should be handled");

        let terminal = control
            .take_idle_terminal()
            .expect("terminal state should be readable")
            .expect("terminal should remain stored");
        assert_eq!(terminal.selected_index, 1);
    }

    #[test]
    fn continuous_session_exposes_cancellation_to_active_runs() {
        let (session, control) = RunManySession::continuous();

        session.cancel();

        assert!(control.cancellation_token().is_cancelled());
    }

    #[test]
    fn completed_continuous_terminal_can_return_to_waiting() {
        let (_session, control) = RunManySession::continuous();
        let mut terminal = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control),
        );
        terminal.completed = 1;
        terminal.rows.get_mut("one").unwrap().status = TaskRowStatus::Succeeded;

        terminal
            .show_waiting()
            .expect("waiting state should render without a terminal");

        assert!(terminal.waiting);
        assert_eq!(terminal.rows["one"].status, TaskRowStatus::Waiting);
    }

    #[test]
    fn continuous_control_surfaces_watch_errors_in_the_idle_terminal() {
        let (_session, control) = RunManySession::continuous();
        let terminal = RunManyTerminal::new_with_control(
            &["one".to_string()],
            Target::Build,
            BTreeMap::new(),
            1,
            Some(control.clone()),
        );
        control
            .store_idle_terminal(terminal)
            .expect("terminal should be stored");

        control
            .show_watch_error("invalid gomo.toml")
            .expect("watch error should be shown");

        let terminal = control
            .take_idle_terminal()
            .expect("terminal should be readable")
            .expect("terminal should remain stored");
        assert_eq!(terminal.watch_error.as_deref(), Some("invalid gomo.toml"));
    }
}
