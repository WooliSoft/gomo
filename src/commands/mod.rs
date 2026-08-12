use std::process;

pub(crate) mod affected;
pub(crate) mod deps;
pub(crate) mod dev;
pub(crate) mod doctor;
pub(crate) mod explain;
pub(crate) mod graph;
pub(crate) mod init;
pub(crate) mod projects;
pub(crate) mod reset;
pub(crate) mod run;
pub(crate) mod task;
pub(crate) mod vendor;
pub(crate) mod watch;
pub(crate) mod watch_support;

use crate::ui::surface::RenderSurface;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OutputOptions {
    pub(crate) json: bool,
    pub(crate) ci: bool,
    pub(crate) tui: bool,
    pub(crate) terminal_width: Option<u16>,
    /// When set, nested work streams here and must not open its own TUI.
    pub(crate) surface: Option<RenderSurface>,
}

impl PartialEq for OutputOptions {
    fn eq(&self, other: &Self) -> bool {
        self.json == other.json
            && self.ci == other.ci
            && self.tui == other.tui
            && self.terminal_width == other.terminal_width
            && self.surface.is_some() == other.surface.is_some()
            && self
                .surface
                .as_ref()
                .zip(other.surface.as_ref())
                .is_none_or(|(left, right)| left.step_id() == right.step_id())
    }
}

impl Eq for OutputOptions {}

impl OutputOptions {
    pub(crate) fn without_tui(self) -> Self {
        Self { tui: false, ..self }
    }

    pub(crate) fn with_surface(self, surface: RenderSurface) -> Self {
        Self {
            tui: false,
            surface: Some(surface),
            ..self
        }
    }
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
