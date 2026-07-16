use std::process;

pub(crate) mod affected;
pub(crate) mod deps;
pub(crate) mod doctor;
pub(crate) mod explain;
pub(crate) mod graph;
pub(crate) mod init;
pub(crate) mod projects;
pub(crate) mod reset;
pub(crate) mod run;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OutputOptions {
    pub(crate) json: bool,
    pub(crate) ci: bool,
    pub(crate) tui: bool,
    pub(crate) terminal_width: Option<u16>,
}

impl CommandOutput {
    pub(crate) fn success(stdout: String) -> Self {
        Self {
            stdout,
            exit_code: 0,
        }
    }

    pub(crate) fn with_exit_code(stdout: String, exit_code: i32) -> Self {
        Self { stdout, exit_code }
    }

    pub(crate) fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

pub(crate) fn exit_code_from_status(status: process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
