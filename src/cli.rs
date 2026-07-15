use anyhow::{Result, bail};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, ColorChoice, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use std::env;
use std::io::IsTerminal;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::commands::{self, CommandOutput, OutputOptions};
use crate::runner::{CommandOptions, Target};

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Yellow.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::Red.on_default().effects(Effects::BOLD));

#[derive(Debug, Parser)]
#[command(
    name = "gomo",
    version,
    about = "Monorepo tooling for Gleam packages",
    color = ColorChoice::Auto,
    styles = HELP_STYLES
)]
struct Cli {
    /// Force commands to run without reading from or writing to the local cache.
    #[arg(long, global = true)]
    no_cache: bool,
    /// Do not restore cached task outputs; successful builds can still refresh cache entries.
    #[arg(long, global = true)]
    no_restore: bool,
    /// Maximum number of tasks to run concurrently.
    #[arg(long, global = true, value_name = "n|auto")]
    parallel: Option<commands::run::Parallelism>,
    /// Render command output as JSON where supported.
    #[arg(long, global = true)]
    json: bool,
    /// Disable rich terminal output for CI-friendly logs.
    #[arg(long, global = true)]
    ci: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a target for affected projects.
    Affected {
        /// Built-in target to run.
        #[arg(long, value_enum)]
        target: Target,
        /// Changed files, relative to the workspace root. Accepts comma-separated or repeated values.
        #[arg(long, value_delimiter = ',', num_args = 1.., value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Compare the current worktree against this VCS ref.
        #[arg(long, value_name = "REF")]
        base: Option<String>,
    },
    /// Run `gleam build` for selected projects.
    Build {
        #[command(flatten)]
        selection: ProjectSelectionArgs,
    },
    /// Run `gleam clean` for selected projects.
    Clean {
        #[command(flatten)]
        selection: ProjectSelectionArgs,
    },
    /// Run `gleam format` for selected projects.
    Format {
        /// Check whether files are already formatted without rewriting them.
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        selection: ProjectSelectionArgs,
    },
    /// Run `gleam test` for selected projects.
    Test {
        #[command(flatten)]
        selection: ProjectSelectionArgs,
    },
    /// Validate the current workspace and dependency graph.
    Doctor,
    /// Inspect resolved dependency versions from Gleam manifest.toml files.
    Deps {
        #[command(subcommand)]
        command: DepsCommands,
    },
    /// Explain the deterministic cache key for a project task.
    Explain {
        /// Built-in target to explain.
        #[arg(long, value_enum)]
        target: Target,
        /// Project name to explain.
        #[arg(long)]
        project: String,
    },
    /// Inspect the workspace dependency graph.
    Graph,
    /// List workspace projects.
    Projects,
    /// Reset local Gomo state intentionally.
    Reset {
        /// Remove only the configured local cache directory.
        #[arg(long)]
        only_cache: bool,
    },
    /// Run a target for one project.
    Run {
        /// Built-in target to run.
        #[arg(long, value_enum)]
        target: Target,
        /// Project name to run.
        #[arg(long)]
        project: String,
        /// Include all upstream workspace dependencies.
        #[arg(long)]
        with_deps: bool,
    },
    /// Run a target for many projects.
    RunMany {
        /// Built-in target to run.
        #[arg(long, value_enum)]
        target: Target,
        /// Run every discovered project.
        #[arg(long)]
        all: bool,
        /// Run one project.
        #[arg(long)]
        project: Option<String>,
        /// Run a comma-separated or repeated project list.
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        projects: Vec<String>,
        /// Include all upstream workspace dependencies.
        #[arg(long)]
        with_deps: bool,
    },
    /// Generate shell completion scripts.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
enum DepsCommands {
    /// Check resolved dependency versions across project manifest.toml files.
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct ProjectSelectionArgs {
    /// Project name to run.
    #[arg(value_name = "PROJECT")]
    positional_project: Option<String>,
    /// Run every discovered project.
    #[arg(long)]
    all: bool,
    /// Run one project.
    #[arg(long)]
    project: Option<String>,
    /// Run a comma-separated or repeated project list.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    projects: Vec<String>,
    /// Include all upstream workspace dependencies.
    #[arg(long)]
    with_deps: bool,
}

/// Parse CLI arguments and run the requested command.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let stdout = execute_from_with_terminal(Cli::parse(), &cwd, should_use_rich_output())?;
    print!("{}", stdout.stdout);
    io::stdout().flush()?;
    if !stdout.is_success() {
        std::process::exit(stdout.exit_code);
    }
    Ok(())
}

#[cfg(test)]
fn execute_from(cli: Cli, cwd: &Path) -> Result<CommandOutput> {
    execute_from_with_terminal(cli, cwd, false)
}

fn execute_from_with_terminal(
    cli: Cli,
    cwd: &Path,
    interactive_terminal: bool,
) -> Result<CommandOutput> {
    let cache_options = commands::run::CacheOptions {
        no_cache: cli.no_cache,
        no_restore: cli.no_restore,
    };
    let parallelism = cli
        .parallel
        .unwrap_or(commands::run::Parallelism::WorkspaceDefault);
    let output_options = OutputOptions {
        json: cli.json,
        ci: cli.ci || cli.json || !interactive_terminal,
        tui: interactive_terminal && !cli.ci && !cli.json,
        terminal_width: terminal_width(interactive_terminal && !cli.ci && !cli.json),
    };

    match cli.command {
        Some(Commands::Build { selection }) => run_shorthand(
            cwd,
            Target::Build,
            selection,
            CommandOptions::default(),
            cache_options,
            parallelism,
            output_options,
        ),
        Some(Commands::Affected {
            target,
            files,
            base,
        }) => commands::affected::run(
            cwd,
            commands::affected::AffectedRequest {
                target,
                files,
                base,
                parallelism,
            },
            cache_options,
            output_options,
        ),
        Some(Commands::Clean { selection }) => run_shorthand(
            cwd,
            Target::Clean,
            selection,
            CommandOptions::default(),
            cache_options,
            parallelism,
            output_options,
        ),
        Some(Commands::Format { check, selection }) => run_shorthand(
            cwd,
            Target::Format,
            selection,
            CommandOptions {
                format_check: check,
            },
            cache_options,
            parallelism,
            output_options,
        ),
        Some(Commands::Doctor) => commands::doctor::run(cwd, output_options),
        Some(Commands::Deps {
            command: DepsCommands::Check,
        }) => commands::deps::run(cwd, output_options),
        Some(Commands::Explain { target, project }) => commands::explain::run(
            cwd,
            commands::explain::ExplainRequest { target, project },
            output_options,
        ),
        Some(Commands::Test { selection }) => run_shorthand(
            cwd,
            Target::Test,
            selection,
            CommandOptions::default(),
            cache_options,
            parallelism,
            output_options,
        ),
        Some(Commands::Graph) => commands::graph::run(cwd, output_options),
        Some(Commands::Projects) => commands::projects::run(cwd, output_options),
        Some(Commands::Reset { only_cache }) => commands::reset::run(
            cwd,
            commands::reset::ResetRequest { only_cache },
            output_options,
        ),
        Some(Commands::Run {
            target,
            project,
            with_deps,
        }) => commands::run::run(
            cwd,
            commands::run::RunRequest {
                target,
                command_options: CommandOptions::default(),
                selection: commands::run::ProjectSelection::Project(project),
                with_deps,
                parallelism,
            },
            cache_options,
            output_options,
        ),
        Some(Commands::RunMany {
            target,
            all,
            project,
            projects,
            with_deps,
        }) => commands::run::run(
            cwd,
            commands::run::RunRequest {
                target,
                command_options: CommandOptions::default(),
                selection: run_many_selection(all, project, projects)?,
                with_deps,
                parallelism,
            },
            cache_options,
            output_options,
        ),
        Some(Commands::Completions { shell }) => generate_completions(shell),
        None => {
            let mut command = Cli::command();
            let help = command.render_help();
            let help = if output_options.ci {
                help.to_string()
            } else {
                help.ansi().to_string()
            };
            Ok(CommandOutput::success(help))
        }
    }
}

fn should_use_rich_output() -> bool {
    rich_output_enabled(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        env::var("TERM").ok().as_deref(),
        env::var_os("NO_COLOR").is_some(),
        env::var_os("CI").is_some() || crate::ui::is_agent_environment(),
    )
}

fn rich_output_enabled(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    term: Option<&str>,
    no_color: bool,
    ci: bool,
) -> bool {
    stdin_is_terminal && stdout_is_terminal && term != Some("dumb") && !no_color && !ci
}

fn terminal_width(enabled: bool) -> Option<u16> {
    if !enabled {
        return None;
    }

    crossterm::terminal::size()
        .ok()
        .map(|(width, _height)| width)
}

fn run_shorthand(
    cwd: &Path,
    target: Target,
    args: ProjectSelectionArgs,
    command_options: CommandOptions,
    cache_options: commands::run::CacheOptions,
    parallelism: commands::run::Parallelism,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let selection = shorthand_selection(target.as_str(), &args)?;
    commands::run::run(
        cwd,
        commands::run::RunRequest {
            target,
            command_options,
            selection,
            with_deps: args.with_deps,
            parallelism,
        },
        cache_options,
        output_options,
    )
}

fn shorthand_selection(
    command: &str,
    args: &ProjectSelectionArgs,
) -> Result<commands::run::ProjectSelection> {
    let selected_modes = usize::from(args.positional_project.is_some())
        + usize::from(args.all)
        + usize::from(args.project.is_some())
        + usize::from(!args.projects.is_empty());

    if selected_modes > 1 {
        bail!("{command} accepts only one of <PROJECT>, --all, --project, or --projects");
    }

    if selected_modes == 0 || args.all {
        return Ok(commands::run::ProjectSelection::All);
    }
    if let Some(project) = &args.positional_project {
        return Ok(commands::run::ProjectSelection::Project(project.clone()));
    }
    if let Some(project) = &args.project {
        return Ok(commands::run::ProjectSelection::Project(project.clone()));
    }

    Ok(commands::run::ProjectSelection::Projects(
        args.projects.clone(),
    ))
}

fn run_many_selection(
    all: bool,
    project: Option<String>,
    projects: Vec<String>,
) -> Result<commands::run::ProjectSelection> {
    let selected_modes =
        usize::from(all) + usize::from(project.is_some()) + usize::from(!projects.is_empty());

    if selected_modes == 0 {
        bail!("run-many requires exactly one of --all, --project, or --projects");
    }
    if selected_modes > 1 {
        bail!("run-many accepts only one of --all, --project, or --projects");
    }

    if all {
        return Ok(commands::run::ProjectSelection::All);
    }
    if let Some(project) = project {
        return Ok(commands::run::ProjectSelection::Project(project));
    }

    Ok(commands::run::ProjectSelection::Projects(projects))
}

fn generate_completions(shell: Shell) -> Result<CommandOutput> {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, "gomo", &mut buffer);
    Ok(CommandOutput::success(String::from_utf8(buffer)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use std::fs;

    use crate::test_support::TestWorkspace;

    fn execute_args(args: &[&str]) -> Result<String> {
        let cli = Cli::try_parse_from(std::iter::once("gomo").chain(args.iter().copied()))?;
        let cwd = std::env::current_dir()?;
        Ok(execute_from(cli, &cwd)?.stdout)
    }

    #[test]
    fn prints_help_with_no_args() {
        let stdout = execute_args(&[]).expect("no args should render help");

        assert!(!stdout.contains("\x1b["));
        assert!(stdout.contains("Usage:"));
        assert!(stdout.contains("affected"));
        assert!(stdout.contains("build"));
        assert!(stdout.contains("clean"));
        assert!(stdout.contains("completions"));
        assert!(stdout.contains("deps"));
        assert!(stdout.contains("doctor"));
        assert!(stdout.contains("explain"));
        assert!(stdout.contains("format"));
        assert!(stdout.contains("graph"));
        assert!(stdout.contains("projects"));
        assert!(stdout.contains("reset"));
        assert!(stdout.contains("run"));
        assert!(stdout.contains("run-many"));
        assert!(stdout.contains("test"));
    }

    #[test]
    fn interactive_help_uses_rich_output() {
        let cli = Cli::try_parse_from(["gomo"]).expect("args should parse");
        let stdout = execute_from_with_terminal(cli, Path::new("."), true)
            .expect("no args should render help")
            .stdout;

        assert!(stdout.contains("\x1b["));
        assert!(strip_ansi(&stdout).contains("Usage:"));
    }

    #[test]
    fn rich_output_requires_a_color_capable_interactive_terminal() {
        assert!(rich_output_enabled(
            true,
            true,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!rich_output_enabled(
            false,
            true,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!rich_output_enabled(
            true,
            false,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!rich_output_enabled(true, true, Some("dumb"), false, false));
        assert!(!rich_output_enabled(
            true,
            true,
            Some("xterm-256color"),
            true,
            false
        ));
        assert!(!rich_output_enabled(
            true,
            true,
            Some("xterm-256color"),
            false,
            true
        ));
    }

    #[test]
    fn generates_shell_completions() {
        let stdout =
            execute_args(&["completions", "bash"]).expect("completion script should be generated");

        assert!(stdout.contains("_gomo"));
        assert!(stdout.contains("affected"));
        assert!(stdout.contains("build"));
        assert!(stdout.contains("clean"));
        assert!(stdout.contains("completions"));
        assert!(stdout.contains("deps"));
        assert!(stdout.contains("doctor"));
        assert!(stdout.contains("explain"));
        assert!(stdout.contains("format"));
        assert!(stdout.contains("graph"));
        assert!(stdout.contains("projects"));
        assert!(stdout.contains("reset"));
        assert!(stdout.contains("run"));
        assert!(stdout.contains("run-many"));
        assert!(stdout.contains("test"));
    }

    #[test]
    fn prints_help_with_flag() {
        let error = Cli::try_parse_from(["gomo", "--help"]).expect_err("help exits through clap");
        let rendered = error.render().ansi().to_string();
        let visible = strip_ansi(&rendered);

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(rendered.contains("\x1b["));
        assert!(visible.contains("Usage: gomo [OPTIONS] [COMMAND]"));
    }

    #[test]
    fn prints_version() {
        let error =
            Cli::try_parse_from(["gomo", "--version"]).expect_err("version exits through clap");

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string(),
            format!("gomo {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    fn strip_ansi(text: &str) -> String {
        let mut stripped = String::new();
        let mut chars = text.chars();

        while let Some(char) = chars.next() {
            if char == '\x1b' {
                for escaped in chars.by_ref() {
                    if escaped == 'm' {
                        break;
                    }
                }
            } else {
                stripped.push(char);
            }
        }

        stripped
    }

    #[test]
    fn lists_discovered_projects() {
        let test_workspace = TestWorkspace::new("gomo-cli-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
target = "javascript"

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

        let nested_dir = test_workspace.path().join("apps/demo/src");
        fs::create_dir_all(&nested_dir).expect("nested dir should be created");
        let cli = Cli::try_parse_from(["gomo", "projects"]).expect("args should parse");
        let stdout = execute_from_with_terminal(cli, &nested_dir, true)
            .expect("projects should be listed")
            .stdout;

        assert!(stdout.contains("\x1b[1;36m"));
        assert!(stdout.contains("Name"));
        assert!(stdout.contains("Target"));
        assert!(stdout.contains("Root"));
        assert!(stdout.contains("demo"));
        assert!(stdout.contains("javascript"));
        assert!(stdout.contains("apps/demo"));
        assert!(stdout.contains("shared"));
        assert!(stdout.contains("erlang"));
        assert!(!stdout.contains("../../libs/shared"));
    }

    #[test]
    fn lists_projects_as_json() {
        let test_workspace = TestWorkspace::new("gomo-cli-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let cli = Cli::try_parse_from(["gomo", "--json", "projects"]).expect("args should parse");
        let stdout = execute_from(cli, test_workspace.path())
            .expect("projects JSON should be listed")
            .stdout;
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON should parse");

        assert_eq!(value["projects"][0]["name"], "shared");
        assert_eq!(value["projects"][0]["root"], "libs/shared");
    }

    #[test]
    fn ci_projects_output_avoids_rich_terminal_sequences() {
        let test_workspace = TestWorkspace::new("gomo-cli-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let cli = Cli::try_parse_from(["gomo", "--ci", "projects"]).expect("args should parse");
        let stdout = execute_from_with_terminal(cli, test_workspace.path(), true)
            .expect("CI projects should be listed")
            .stdout;

        assert!(stdout.contains("Projects"));
        assert!(stdout.contains("shared"));
        assert!(!stdout.contains("\x1b["));
        assert!(!stdout.contains("│"));
    }

    #[test]
    fn non_interactive_inspection_commands_use_plain_output() {
        let test_workspace = TestWorkspace::new("gomo-cli-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file(
            "libs/shared/manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], source = "hex" },
]
"#,
        );

        for args in [
            &["deps", "check"][..],
            &["doctor"][..],
            &["graph"][..],
            &["projects"][..],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("gomo").chain(args.iter().copied()))
                .expect("args should parse");
            let stdout = execute_from(cli, test_workspace.path())
                .expect("inspection command should run")
                .stdout;

            assert!(!stdout.contains("\x1b["), "{args:?} emitted ANSI output");
            assert!(!stdout.contains('╭'), "{args:?} emitted a rich border");
        }
    }

    #[test]
    fn prints_workspace_graph() {
        let test_workspace = TestWorkspace::new("gomo-cli-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
target = "javascript"

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

        let cli = Cli::try_parse_from(["gomo", "graph"]).expect("args should parse");
        let stdout = execute_from_with_terminal(cli, test_workspace.path(), true)
            .expect("graph should be listed")
            .stdout;

        assert!(stdout.contains("Dependency Graph"));
        assert!(stdout.contains("○"));
        assert!(stdout.contains("shared"));
        assert!(stdout.contains("demo"));
        assert!(!stdout.contains("depends on:"));
    }

    #[test]
    fn rejects_unknown_command() {
        let error =
            Cli::try_parse_from(["gomo", "missing"]).expect_err("unknown command should fail");

        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert_eq!(error.exit_code(), 2);
        assert!(
            error
                .to_string()
                .contains("unrecognized subcommand 'missing'")
        );
    }

    #[test]
    fn validates_run_many_project_selection() {
        let error =
            run_many_selection(false, None, Vec::new()).expect_err("missing selection should fail");
        assert!(
            error
                .to_string()
                .contains("run-many requires exactly one of")
        );

        let error = run_many_selection(true, Some("demo".to_string()), Vec::new())
            .expect_err("multiple selections should fail");
        assert!(error.to_string().contains("run-many accepts only one"));

        assert_eq!(
            run_many_selection(false, None, vec!["demo".to_string(), "shared".to_string()])
                .expect("projects should be accepted"),
            commands::run::ProjectSelection::Projects(vec![
                "demo".to_string(),
                "shared".to_string()
            ])
        );
    }

    #[test]
    fn parses_affected_files() {
        let cli = Cli::try_parse_from([
            "gomo",
            "affected",
            "--target",
            "test",
            "--files",
            "libs/shared/src/main.gleam,apps/demo/src/main.gleam",
        ])
        .expect("affected args should parse");

        match cli.command {
            Some(Commands::Affected {
                target,
                files,
                base,
            }) => {
                assert_eq!(target, Target::Test);
                assert_eq!(base, None);
                assert_eq!(
                    files,
                    [
                        PathBuf::from("libs/shared/src/main.gleam"),
                        PathBuf::from("apps/demo/src/main.gleam")
                    ]
                );
            }
            other => panic!("expected affected command, got {other:?}"),
        }
    }

    #[test]
    fn parses_affected_base() {
        let cli = Cli::try_parse_from(["gomo", "affected", "--target", "test", "--base", "main"])
            .expect("affected args should parse");

        match cli.command {
            Some(Commands::Affected {
                target,
                files,
                base,
            }) => {
                assert_eq!(target, Target::Test);
                assert!(files.is_empty());
                assert_eq!(base, Some("main".to_string()));
            }
            other => panic!("expected affected command, got {other:?}"),
        }
    }

    #[test]
    fn parses_explain_command() {
        let cli =
            Cli::try_parse_from(["gomo", "explain", "--target", "test", "--project", "shared"])
                .expect("explain args should parse");

        match cli.command {
            Some(Commands::Explain { target, project }) => {
                assert_eq!(target, Target::Test);
                assert_eq!(project, "shared");
            }
            other => panic!("expected explain command, got {other:?}"),
        }
    }

    #[test]
    fn parses_deps_check_command() {
        let cli = Cli::try_parse_from(["gomo", "deps", "check"]).expect("deps args should parse");

        match cli.command {
            Some(Commands::Deps {
                command: DepsCommands::Check,
            }) => {}
            other => panic!("expected deps check command, got {other:?}"),
        }
    }

    #[test]
    fn parses_format_check() {
        let cli = Cli::try_parse_from(["gomo", "format", "--check", "--project", "shared"])
            .expect("format check args should parse");

        match cli.command {
            Some(Commands::Format { check, selection }) => {
                assert!(check);
                assert_eq!(selection.project, Some("shared".to_string()));
            }
            other => panic!("expected format command, got {other:?}"),
        }
    }

    #[test]
    fn parses_reset_only_cache() {
        let cli = Cli::try_parse_from(["gomo", "reset", "--only-cache"])
            .expect("reset args should parse");

        match cli.command {
            Some(Commands::Reset { only_cache }) => assert!(only_cache),
            other => panic!("expected reset command, got {other:?}"),
        }
    }

    #[test]
    fn validates_shorthand_project_selection() {
        assert_eq!(
            shorthand_selection(
                "build",
                &ProjectSelectionArgs {
                    positional_project: None,
                    all: false,
                    project: None,
                    projects: Vec::new(),
                    with_deps: false,
                },
            )
            .expect("missing selection should default to all"),
            commands::run::ProjectSelection::All
        );

        assert_eq!(
            shorthand_selection(
                "test",
                &ProjectSelectionArgs {
                    positional_project: None,
                    all: true,
                    project: None,
                    projects: Vec::new(),
                    with_deps: false,
                },
            )
            .expect("explicit all should be accepted"),
            commands::run::ProjectSelection::All
        );

        let error = shorthand_selection(
            "build",
            &ProjectSelectionArgs {
                positional_project: Some("demo".to_string()),
                all: true,
                project: None,
                projects: Vec::new(),
                with_deps: false,
            },
        )
        .expect_err("multiple selections should fail");
        assert!(error.to_string().contains("build accepts only one"));

        assert_eq!(
            shorthand_selection(
                "test",
                &ProjectSelectionArgs {
                    positional_project: None,
                    all: false,
                    project: Some("demo".to_string()),
                    projects: Vec::new(),
                    with_deps: false,
                },
            )
            .expect("flagged project should be accepted"),
            commands::run::ProjectSelection::Project("demo".to_string())
        );

        assert_eq!(
            shorthand_selection(
                "clean",
                &ProjectSelectionArgs {
                    positional_project: Some("demo".to_string()),
                    all: false,
                    project: None,
                    projects: Vec::new(),
                    with_deps: false,
                },
            )
            .expect("positional project should be accepted"),
            commands::run::ProjectSelection::Project("demo".to_string())
        );
    }
}
