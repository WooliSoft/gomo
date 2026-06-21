use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Wrap},
};

use crate::commands::run::{TaskCacheStatus, TaskOutcome, TaskStatus, TaskSummary};
use crate::runner::Target;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(crate) struct RunManyTerminal {
    target: Target,
    command_displays: BTreeMap<String, String>,
    project_names: Vec<String>,
    rows: BTreeMap<String, TaskRow>,
    logs: BTreeMap<String, String>,
    selected_index: usize,
    fullscreen_logs: bool,
    parallelism: usize,
    started_at: Instant,
    spinner_frame: usize,
    completed: usize,
    succeeded: usize,
    failed: usize,
    cached: usize,
    skipped: usize,
    summary: Option<TaskSummary>,
    exit_prompt: Option<ExitPrompt>,
    terminal: Option<TuiTerminal>,
    raw_mode_enabled: bool,
}

impl RunManyTerminal {
    pub(crate) fn new(
        project_names: &[String],
        target: Target,
        command_displays: BTreeMap<String, String>,
        parallelism: usize,
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
            command_displays,
            project_names: project_names.to_vec(),
            rows,
            logs: BTreeMap::new(),
            selected_index: 0,
            fullscreen_logs: false,
            parallelism,
            started_at: Instant::now(),
            spinner_frame: 0,
            completed: 0,
            succeeded: 0,
            failed: 0,
            cached: 0,
            skipped: 0,
            summary: None,
            exit_prompt: None,
            terminal: None,
            raw_mode_enabled: false,
        }
    }

    pub(crate) fn start(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.raw_mode_enabled = true;

        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
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

    pub(crate) fn task_skipped(&mut self, outcome: &TaskOutcome) -> io::Result<()> {
        self.handle_input()?;
        self.finish_row(outcome);
        self.draw()
    }

    pub(crate) fn tick(&mut self) -> io::Result<()> {
        self.handle_input()?;
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        self.draw()
    }

    pub(crate) fn finish(&mut self, summary: &TaskSummary) -> io::Result<RunManyExit> {
        self.summary = Some(summary.clone());
        self.draw()?;
        if is_agent_environment() {
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
        if let Some(mut terminal) = self.terminal.take() {
            execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
            terminal.show_cursor()?;
        }
        if self.raw_mode_enabled {
            disable_raw_mode()?;
            self.raw_mode_enabled = false;
        }
        io::stdout().flush()
    }

    fn wait_for_exit_confirmation(&mut self) -> io::Result<RunManyExit> {
        loop {
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if self.handle_key(key.code) {
                        return Ok(RunManyExit::UserExited);
                    }
                    self.draw()?;
                }
            }
        }
    }

    fn handle_input(&mut self) -> io::Result<()> {
        if self.terminal.is_none() {
            return Ok(());
        }

        while event::poll(Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                self.handle_key(key.code);
            }
        }

        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Enter
                if self.summary.is_some() && self.exit_prompt == Some(ExitPrompt::Cancelled) =>
            {
                self.fullscreen_logs = true;
            }
            KeyCode::Enter if self.summary.is_some() => return true,
            KeyCode::Char('q') if self.summary.is_some() => return true,
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.fullscreen_logs = !self.fullscreen_logs;
            }
            KeyCode::Esc if self.fullscreen_logs => {
                self.fullscreen_logs = false;
            }
            KeyCode::Esc if self.summary.is_some() => return true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_index = self.selected_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.project_names.is_empty() {
                    self.selected_index =
                        (self.selected_index + 1).min(self.project_names.len() - 1);
                }
            }
            KeyCode::Home => self.selected_index = 0,
            KeyCode::End => {
                if !self.project_names.is_empty() {
                    self.selected_index = self.project_names.len() - 1;
                }
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
    Pending,
    Running,
    Succeeded,
    Cached,
    Failed(i32),
    Skipped,
}

fn is_agent_environment() -> bool {
    env::var("AGENT").is_ok_and(|v| v == "1")
        || env::var("OPENCODE").is_ok_and(|v| v == "1")
        || env::var("CODEX_CI").is_ok_and(|v| v == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitPrompt {
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunManyExit {
    AutoExited,
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

fn render(frame: &mut Frame<'_>, app: &RunManyTerminal) {
    if app.fullscreen_logs {
        render_fullscreen_logs(frame, frame.area(), app);
        return;
    }

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(8)])
        .split(frame.area());

    render_body(frame, outer[0], app);
    render_run_summary(frame, outer[1], app);
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &RunManyTerminal) {
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

fn render_log_panel(frame: &mut Frame<'_>, area: Rect, app: &RunManyTerminal) {
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

fn render_fullscreen_logs(frame: &mut Frame<'_>, area: Rect, app: &RunManyTerminal) {
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
    Line::from(title)
}

fn log_lines(app: &RunManyTerminal, max_lines: usize) -> Vec<Line<'static>> {
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
    ansi_log_lines(&output, max_lines.max(1))
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
    line.trim_start().starts_with("[cache hit]")
}

fn is_hidden_cached_output_line(line: &str, command_display: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("==>")
        || trimmed == format!("$ {command_display}")
        || is_hidden_log_metadata(line)
}

fn ansi_log_lines(input: &str, max_lines: usize) -> Vec<Line<'static>> {
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

    if lines.len() > max_lines {
        lines.split_off(lines.len() - max_lines)
    } else {
        lines
    }
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
    let running = app
        .rows
        .values()
        .filter(|row| row.status == TaskRowStatus::Running)
        .count();
    let remaining = app.project_names.len().saturating_sub(app.completed);
    let status_color = if app.failed > 0 {
        Color::Red
    } else if app.summary.is_some() {
        Color::Green
    } else {
        Color::Cyan
    };
    let percentage = if app.project_names.is_empty() {
        100
    } else {
        app.completed * 100 / app.project_names.len()
    };
    let lines = vec![
        Line::from(vec![
            styled("Gomo", status_color, Modifier::BOLD),
            raw(" "),
            styled(app.target.as_str(), Color::White, Modifier::BOLD),
            dim(format!(
                "  {} total  {} running  {} remaining  parallel {}",
                app.project_names.len(),
                running,
                remaining,
                app.parallelism
            )),
        ]),
        Line::from(""),
        Line::from(vec![
            dim("Progress"),
            raw("  "),
            styled(
                format!("{} / {} complete", app.completed, app.project_names.len()),
                status_color,
                Modifier::BOLD,
            ),
            dim(format!("  {percentage}%")),
        ]),
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
        Line::from(vec![
            dim("Elapsed"),
            raw("   "),
            raw(pretty_duration(app.started_at.elapsed())),
        ]),
        Line::from(vec![dim("Keys"), raw("      "), raw(key_hint(app))]),
    ];
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

fn key_hint(app: &RunManyTerminal) -> &'static str {
    if app.summary.is_some() {
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
        TaskRowStatus::Pending => vec![dim("queued")],
        TaskRowStatus::Running => vec![
            styled(
                SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
                Color::Cyan,
                Modifier::BOLD,
            ),
            raw(" running"),
        ],
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
    fn renders_ansi_colored_logs_as_styled_spans() {
        let lines = ansi_log_lines("\x1b[32m.\x1b[39m 132 passed\n", 10);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, ".");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].spans[1].content, " 132 passed");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Reset));

        let lines = ansi_log_lines("\x1b]0;title\x07ok", 10);
        assert_eq!(lines[0].spans[0].content, "ok");
    }

    #[test]
    fn hides_cache_hit_metadata_from_logs() {
        assert_eq!(
            filter_log_metadata("[cache hit] abc123\nreal output\n"),
            "real output"
        );
    }

    #[test]
    fn cached_log_output_keeps_only_replayed_command_output() {
        assert_eq!(
            cached_log_output(
                "==> web_app:build (apps/web_app)\n$ gleam build\n[cache hit] abc123\ncompiled\n",
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
            }],
        };
        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("with 1 failed task"));
        assert!(rendered.contains("web_app:build"));
    }
}
