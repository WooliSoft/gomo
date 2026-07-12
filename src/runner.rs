use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::commands::exit_code_from_status;
use crate::workspace::Project;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum Target {
    Build,
    Clean,
    Format,
    Test,
}

impl Target {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Clean => "clean",
            Self::Format => "format",
            Self::Test => "test",
        }
    }

    pub(crate) fn command_display(self) -> String {
        format!("gleam {}", self.as_str())
    }

    pub(crate) fn supports_cache(self) -> bool {
        matches!(self, Self::Build | Self::Format | Self::Test)
    }

    pub(crate) fn supports_tui(self) -> bool {
        matches!(self, Self::Build | Self::Clean | Self::Format | Self::Test)
    }

    pub(crate) fn refreshes_cache_key_after_success(self) -> bool {
        matches!(self, Self::Format)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CommandOptions {
    pub(crate) format_check: bool,
}

impl CommandOptions {
    pub(crate) fn command_display(self, project: &Project, target: Target) -> Result<String> {
        Ok(resolve_command(project, target, self)?.display)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedCommand {
    pub(crate) display: String,
    program: String,
    args: Vec<String>,
}

pub(crate) fn resolve_command(
    project: &Project,
    target: Target,
    options: CommandOptions,
) -> Result<ResolvedCommand> {
    if let Some(command) = configured_command(project, target, options) {
        return parse_custom_command(project, target, command);
    }

    let mut args = vec![target.as_str().to_string()];
    if options.format_check && target == Target::Format {
        args.push("--check".to_string());
    }
    let display = std::iter::once("gleam".to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");

    Ok(ResolvedCommand {
        display,
        program: "gleam".to_string(),
        args,
    })
}

fn configured_command(project: &Project, target: Target, options: CommandOptions) -> Option<&str> {
    let config = project.gomo_targets.get(target.as_str())?;
    if target == Target::Format && options.format_check {
        config.check_command.as_deref()
    } else {
        config.command.as_deref()
    }
}

fn parse_custom_command(
    project: &Project,
    target: Target,
    command: &str,
) -> Result<ResolvedCommand> {
    let command = command.trim();
    if command.is_empty() {
        bail!(
            "custom command for `{target}` in {} must not be empty",
            project.manifest_path.display()
        );
    }

    Ok(ResolvedCommand {
        display: command.to_string(),
        program: "sh".to_string(),
        args: vec!["-c".to_string(), command.to_string()],
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskExecution {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
}

impl TaskExecution {
    #[cfg(test)]
    pub(crate) fn success(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code: 0,
        }
    }

    pub(crate) fn failure(
        exit_code: i32,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    pub(crate) fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

pub(crate) trait CommandRunner: Sync {
    fn run(&self, project: &Project, target: Target, options: CommandOptions) -> TaskExecution;

    fn run_with_output(
        &self,
        project: &Project,
        target: Target,
        options: CommandOptions,
        output: &mut dyn FnMut(&str),
    ) -> TaskExecution {
        let execution = self.run(project, target, options);
        emit_stream(output, &execution.stdout);
        emit_stream(output, &execution.stderr);
        execution
    }
}

pub(crate) struct GleamCommandRunner;

impl CommandRunner for GleamCommandRunner {
    fn run(&self, project: &Project, target: Target, options: CommandOptions) -> TaskExecution {
        let command_display = match options.command_display(project, target) {
            Ok(command_display) => command_display,
            Err(error) => return TaskExecution::failure(127, "", format!("{error}\n")),
        };
        match task_command(project, target, options).and_then(|mut command| {
            command
                .output()
                .with_context(|| format!("failed to run `{command_display}`"))
        }) {
            Ok(output) => TaskExecution {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: exit_code_from_status(output.status),
            },
            Err(error) => TaskExecution::failure(
                127,
                "",
                format!(
                    "failed to run `{}` in {}: {error}\n",
                    command_display,
                    project.root.display()
                ),
            ),
        }
    }

    fn run_with_output(
        &self,
        project: &Project,
        target: Target,
        options: CommandOptions,
        output: &mut dyn FnMut(&str),
    ) -> TaskExecution {
        let command_display = match options.command_display(project, target) {
            Ok(command_display) => command_display,
            Err(error) => return TaskExecution::failure(127, "", format!("{error}\n")),
        };
        let mut command = match task_command(project, target, options) {
            Ok(command) => command,
            Err(error) => return TaskExecution::failure(127, "", format!("{error}\n")),
        };
        let mut child = match command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return TaskExecution::failure(
                    127,
                    "",
                    format!(
                        "failed to run `{}` in {}: {error}\n",
                        command_display,
                        project.root.display()
                    ),
                );
            }
        };

        let Some(stdout) = child.stdout.take() else {
            return TaskExecution::failure(1, "", "failed to capture stdout\n");
        };
        let Some(stderr) = child.stderr.take() else {
            return TaskExecution::failure(1, "", "failed to capture stderr\n");
        };

        let (sender, receiver) = mpsc::channel::<ProcessEvent>();
        spawn_reader(stdout, StreamKind::Stdout, sender.clone());
        spawn_reader(stderr, StreamKind::Stderr, sender.clone());
        thread::spawn(move || {
            let _ = sender.send(ProcessEvent::Exit(child.wait().map(exit_code_from_status)));
        });

        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut readers_finished = 0usize;
        let mut exit_code = None;

        while readers_finished < 2 || exit_code.is_none() {
            match receiver.recv() {
                Ok(ProcessEvent::Chunk(StreamKind::Stdout, chunk)) => {
                    emit_stream(output, &chunk);
                    stdout.push_str(&chunk);
                }
                Ok(ProcessEvent::Chunk(StreamKind::Stderr, chunk)) => {
                    emit_stream(output, &chunk);
                    stderr.push_str(&chunk);
                }
                Ok(ProcessEvent::ReaderDone) => readers_finished += 1,
                Ok(ProcessEvent::Exit(result)) => {
                    exit_code = Some(result.unwrap_or(1));
                }
                Err(_) => break,
            }
        }

        TaskExecution {
            stdout,
            stderr,
            exit_code: exit_code.unwrap_or(1),
        }
    }
}

fn task_command(project: &Project, target: Target, options: CommandOptions) -> Result<Command> {
    let resolved_command = resolve_command(project, target, options)?;
    let mut command = Command::new(&resolved_command.program);
    command.args(&resolved_command.args);
    command.current_dir(&project.root);
    Ok(command)
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

enum ProcessEvent {
    Chunk(StreamKind, String),
    ReaderDone,
    Exit(std::io::Result<i32>),
}

fn spawn_reader(
    stream: impl Read + Send + 'static,
    kind: StreamKind,
    sender: mpsc::Sender<ProcessEvent>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = sender.send(ProcessEvent::Chunk(kind, line.clone()));
                }
                Err(error) => {
                    let _ = sender.send(ProcessEvent::Chunk(
                        kind,
                        format!("failed to read process output: {error}\n"),
                    ));
                    break;
                }
            }
        }
        let _ = sender.send(ProcessEvent::ReaderDone);
    });
}

fn emit_stream(output: &mut dyn FnMut(&str), stream: &str) {
    if stream.is_empty() {
        return;
    }

    output(stream);
    if !stream.ends_with('\n') {
        output("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    fn project_with_manifest(contents: &str) -> crate::workspace::Project {
        let test_workspace = TestWorkspace::new("gomo-runner-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]
"#,
        );
        test_workspace.write_manifest("libs/demo", contents);
        workspace::discover(test_workspace.path())
            .expect("workspace should load")
            .projects
            .into_iter()
            .next()
            .expect("project should exist")
    }

    #[test]
    fn resolves_default_commands() {
        let project = project_with_manifest(
            r#"
name = "demo"
version = "0.1.0"
"#,
        );

        let command = resolve_command(&project, Target::Build, CommandOptions::default())
            .expect("default command should resolve");

        assert_eq!(command.display, "gleam build");
        assert_eq!(command.program, "gleam");
        assert_eq!(command.args, ["build"]);
    }

    #[test]
    fn resolves_custom_build_and_test_commands() {
        let project = project_with_manifest(
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
command = "mise exec -- gleam build"

[tools.gomo.test]
command = "gleam test --target erlang"
"#,
        );

        let build = resolve_command(&project, Target::Build, CommandOptions::default())
            .expect("custom build command should resolve");
        let test = resolve_command(&project, Target::Test, CommandOptions::default())
            .expect("custom test command should resolve");

        assert_eq!(build.display, "mise exec -- gleam build");
        assert_eq!(build.program, "sh");
        assert_eq!(build.args, ["-c", "mise exec -- gleam build"]);
        assert_eq!(test.display, "gleam test --target erlang");
        assert_eq!(test.program, "sh");
        assert_eq!(test.args, ["-c", "gleam test --target erlang"]);
    }

    #[test]
    fn resolves_custom_format_and_format_check_commands() {
        let project = project_with_manifest(
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.format]
command = "mise exec -- gleam format"

[tools.gomo.format.check]
command = "mise exec -- gleam format --check"
"#,
        );

        let format = resolve_command(&project, Target::Format, CommandOptions::default())
            .expect("custom format command should resolve");
        let check = resolve_command(
            &project,
            Target::Format,
            CommandOptions { format_check: true },
        )
        .expect("custom format check command should resolve");

        assert_eq!(format.display, "mise exec -- gleam format");
        assert_eq!(format.program, "sh");
        assert_eq!(format.args, ["-c", "mise exec -- gleam format"]);
        assert_eq!(check.display, "mise exec -- gleam format --check");
        assert_eq!(check.program, "sh");
        assert_eq!(check.args, ["-c", "mise exec -- gleam format --check"]);
    }

    #[test]
    fn format_check_uses_default_when_only_format_inputs_are_configured() {
        let project = project_with_manifest(
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.format]
inputs = ["gleam.toml", "src/**"]
"#,
        );

        let check = resolve_command(
            &project,
            Target::Format,
            CommandOptions { format_check: true },
        )
        .expect("default format check command should resolve");

        assert_eq!(check.display, "gleam format --check");
        assert_eq!(check.program, "gleam");
        assert_eq!(check.args, ["format", "--check"]);
    }

    #[test]
    fn custom_commands_support_shell_operators() {
        let test_workspace = TestWorkspace::new("gomo-runner-shell-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["libs/*"]
"#,
        );
        test_workspace.write_manifest(
            "libs/demo",
            r#"
name = "demo"
version = "0.1.0"

[tools.gomo.build]
command = "printf first > result.txt && printf second >> result.txt"
"#,
        );
        let project = workspace::discover(test_workspace.path())
            .expect("workspace should load")
            .projects
            .into_iter()
            .next()
            .expect("project should exist");

        let execution = GleamCommandRunner.run(&project, Target::Build, CommandOptions::default());

        assert_eq!(execution.exit_code, 0, "{}", execution.stderr);
        assert_eq!(
            std::fs::read_to_string(project.root.join("result.txt"))
                .expect("custom command should write its output"),
            "firstsecond"
        );
    }
}
