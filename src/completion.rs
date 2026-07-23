use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use clap_complete::env::Shells;
use clap_complete::{CompleteEnv, CompletionCandidate, Shell};

use crate::commands::CommandOutput;
use crate::workspace;

const COMPLETE_ENV: &str = "COMPLETE";

pub(crate) fn handle_dynamic_completion(factory: impl Fn() -> clap::Command) {
    CompleteEnv::with_factory(factory).complete();
}

pub(crate) fn project_candidates() -> Vec<CompletionCandidate> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };

    project_names_from(&cwd)
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

pub(crate) fn generate_registration(shell: Shell) -> Result<CommandOutput> {
    let shells = Shells::builtins();
    let shell = shells
        .completer(&shell.to_string())
        .context("unsupported completion shell")?;
    let mut buffer = Vec::new();
    shell.write_registration(COMPLETE_ENV, "gomo", "gomo", "gomo", &mut buffer)?;
    Ok(CommandOutput::success(String::from_utf8(buffer)?))
}

fn project_names_from(cwd: &Path) -> Vec<OsString> {
    let Ok(workspace) = workspace::discover_from(cwd) else {
        return Vec::new();
    };
    let mut names = workspace
        .projects
        .into_iter()
        .map(|project| OsString::from(project.name))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, Command, ValueEnum};
    use clap_complete::ArgValueCandidates;
    use clap_complete::engine::complete;
    use std::fs;

    use crate::test_support::TestWorkspace;

    #[test]
    fn discovers_sorted_project_names_from_nested_directories() {
        let workspace = TestWorkspace::new("gomo-completion-test");
        workspace.write_gomo_config();
        workspace.write_manifest(
            "apps/first",
            r#"
name = "zebra"
version = "0.1.0"
"#,
        );
        workspace.write_manifest(
            "libs/second",
            r#"
name = "alpha"
version = "0.1.0"
"#,
        );
        let nested = workspace.path().join("apps/first/src/nested");
        fs::create_dir_all(&nested).expect("nested directory should be created");

        assert_eq!(
            project_names_from(&nested),
            [OsString::from("alpha"), OsString::from("zebra")]
        );
    }

    #[test]
    fn discovery_failures_produce_no_candidates() {
        let outside_workspace = TestWorkspace::new("gomo-completion-test");
        assert!(project_names_from(outside_workspace.path()).is_empty());

        outside_workspace.write_file("gomo.toml", "not valid toml");
        assert!(project_names_from(outside_workspace.path()).is_empty());
    }

    #[test]
    fn completes_only_the_unfinished_comma_separated_project() {
        let candidates = || {
            ["api", "shared", "web"]
                .into_iter()
                .map(CompletionCandidate::new)
                .collect()
        };
        let mut command = Command::new("gomo").subcommand(
            Command::new("run-many").arg(
                Arg::new("projects")
                    .long("projects")
                    .value_delimiter(',')
                    .num_args(1..)
                    .add(ArgValueCandidates::new(candidates)),
            ),
        );
        let completions = complete(
            &mut command,
            ["gomo", "run-many", "--projects", "web,sh"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            3,
            None,
        )
        .expect("completion should succeed");

        assert_eq!(
            completions
                .iter()
                .map(|candidate| candidate.get_value())
                .collect::<Vec<_>>(),
            [std::ffi::OsStr::new("web,shared")]
        );
    }

    #[test]
    fn project_selecting_arguments_use_dynamic_candidates() {
        let mut command = crate::cli::command();
        command.build();

        for (subcommand, arguments) in [
            ("build", &["positional_project", "project", "projects"][..]),
            ("clean", &["positional_project", "project", "projects"][..]),
            ("format", &["positional_project", "project", "projects"][..]),
            ("test", &["positional_project", "project", "projects"][..]),
            ("explain", &["project"][..]),
            ("run", &["project"][..]),
            ("run-many", &["project", "projects"][..]),
        ] {
            let subcommand = command
                .find_subcommand(subcommand)
                .expect("project-selecting subcommand should exist");
            for argument in arguments {
                let argument = subcommand
                    .get_arguments()
                    .find(|candidate| candidate.get_id() == *argument)
                    .expect("project-selecting argument should exist");
                assert!(
                    argument.get::<ArgValueCandidates>().is_some(),
                    "{} {argument} did not have dynamic project candidates",
                    subcommand.get_name()
                );
            }
        }
    }

    #[test]
    fn static_clap_candidates_remain_available() {
        let mut command = crate::cli::command();
        let targets = complete(
            &mut command,
            ["gomo", "run", "--target", ""]
                .into_iter()
                .map(OsString::from)
                .collect(),
            3,
            None,
        )
        .expect("target completion should succeed");
        let target_values = targets
            .iter()
            .map(|candidate| candidate.get_value())
            .collect::<Vec<_>>();
        assert!(target_values.contains(&std::ffi::OsStr::new("build")));
        assert!(target_values.contains(&std::ffi::OsStr::new("test")));

        let shells = complete(
            &mut command,
            ["gomo", "completions", ""]
                .into_iter()
                .map(OsString::from)
                .collect(),
            2,
            None,
        )
        .expect("shell completion should succeed");
        let shell_values = shells
            .iter()
            .map(|candidate| candidate.get_value())
            .collect::<Vec<_>>();
        for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
            assert!(shell_values.contains(&std::ffi::OsStr::new(shell)));
        }
    }

    #[test]
    fn generates_dynamic_registration_for_every_supported_shell() {
        for shell in Shell::value_variants() {
            let script = generate_registration(*shell)
                .expect("registration should be generated")
                .stdout;

            assert!(!script.is_empty(), "{shell} registration was empty");
            assert!(script.contains("gomo"), "{shell} did not register gomo");
            assert!(
                script.contains(COMPLETE_ENV),
                "{shell} did not enable the dynamic endpoint"
            );
        }
    }
}
