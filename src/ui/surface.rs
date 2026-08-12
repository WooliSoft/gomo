use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cancellation::{CancellationToken, ChildCancellation, configure_process_group};

#[derive(Debug, Clone)]
pub(crate) enum SurfaceEvent {
    Log { step: String, chunk: String },
    StepStarted { step: String },
    StepFinished { step: String, ok: bool },
}

/// Routes nested command output into a parent persistent-task TUI (or CI prefixes).
#[derive(Debug, Clone)]
pub(crate) struct RenderSurface {
    step_id: String,
    events: Arc<Mutex<Sender<SurfaceEvent>>>,
    cancellation: CancellationToken,
    /// When true, also print CI-style prefixed lines to stdout.
    ci: bool,
    color: bool,
}

impl RenderSurface {
    pub(crate) fn channel(
        cancellation: CancellationToken,
        _ci: bool,
    ) -> (
        Sender<SurfaceEvent>,
        Receiver<SurfaceEvent>,
        CancellationToken,
    ) {
        let (tx, rx) = mpsc::channel();
        (tx, rx, cancellation)
    }

    pub(crate) fn new(
        step_id: impl Into<String>,
        events: Sender<SurfaceEvent>,
        cancellation: CancellationToken,
        ci: bool,
    ) -> Self {
        Self {
            step_id: step_id.into(),
            events: Arc::new(Mutex::new(events)),
            cancellation,
            ci,
            color: false,
        }
    }

    pub(crate) fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    pub(crate) fn with_cancellation(&self, cancellation: CancellationToken) -> Self {
        Self {
            step_id: self.step_id.clone(),
            events: Arc::clone(&self.events),
            cancellation,
            ci: self.ci,
            color: self.color,
        }
    }

    pub(crate) fn step_id(&self) -> &str {
        &self.step_id
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn color_enabled(&self) -> bool {
        self.color
    }

    pub(crate) fn mark_started(&self) {
        let _ = self.send(SurfaceEvent::StepStarted {
            step: self.step_id.clone(),
        });
    }

    pub(crate) fn mark_finished(&self, ok: bool) {
        let _ = self.send(SurfaceEvent::StepFinished {
            step: self.step_id.clone(),
            ok,
        });
    }

    pub(crate) fn append_log(&self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if self.ci {
            for line in chunk.split_inclusive('\n') {
                if line.is_empty() {
                    continue;
                }
                print!("[{}] {line}", self.step_id);
                if !line.ends_with('\n') {
                    println!();
                }
            }
            let _ = std::io::stdout().flush();
        }
        let _ = self.send(SurfaceEvent::Log {
            step: self.step_id.clone(),
            chunk: chunk.to_string(),
        });
    }

    fn send(&self, event: SurfaceEvent) -> Result<()> {
        self.events
            .lock()
            .map_err(|_| anyhow::anyhow!("render surface lock was poisoned"))?
            .send(event)
            .map_err(|_| anyhow::anyhow!("render surface closed"))?;
        Ok(())
    }
}

/// Run a long-lived process, streaming stdout/stderr into the surface until exit or cancel.
pub(crate) fn run_piped_persistent(
    mut command: Command,
    surface: &RenderSurface,
    unexpected_success: impl FnOnce() -> String,
) -> Result<String> {
    configure_process_group(&mut command);
    configure_captured_output(&mut command, surface.color);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn persistent step `{}`", surface.step_id()))?;
    stream_child_output(&mut child, surface)?;
    let mut cancellation = ChildCancellation::new(surface.cancellation());
    let status = loop {
        cancellation.poll(&mut child);
        if let Some(status) = child
            .try_wait()
            .context("failed to wait for persistent step")?
        {
            break status;
        }
        if surface.cancellation().is_cancelled() {
            // ChildCancellation already signaled; wait for exit.
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        thread::sleep(Duration::from_millis(20));
    };
    if surface.cancellation().is_cancelled() {
        return Ok(String::new());
    }
    if status.success() {
        bail!("{}", unexpected_success());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal().is_some() {
            return Ok(String::new());
        }
    }
    let code = status.code().unwrap_or(1);
    bail!(
        "persistent step `{}` failed with exit code {code}",
        surface.step_id()
    );
}

fn stream_child_output(child: &mut Child, surface: &RenderSurface) -> Result<()> {
    let stdout = child
        .stdout
        .take()
        .context("persistent step stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("persistent step stderr was not captured")?;
    let surface_out = surface.clone();
    let surface_err = surface.clone();
    thread::spawn(move || pump_pipe(stdout, surface_out));
    thread::spawn(move || pump_pipe(stderr, surface_err));
    Ok(())
}

fn pump_pipe(mut pipe: impl Read, surface: RenderSurface) {
    let mut buffer = [0_u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let chunk = String::from_utf8_lossy(&buffer[..read]);
                surface.append_log(&chunk);
            }
            Err(_) => break,
        }
    }
}

pub(crate) fn configure_captured_output(command: &mut Command, color: bool) {
    if color {
        command.env("FORCE_COLOR", "1").env("CLICOLOR_FORCE", "1");
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn forces_color_for_captured_tui_output() {
        let mut command = Command::new("example");

        configure_captured_output(&mut command, true);

        assert_eq!(command_env(&command, "FORCE_COLOR"), Some(OsStr::new("1")));
        assert_eq!(
            command_env(&command, "CLICOLOR_FORCE"),
            Some(OsStr::new("1"))
        );
    }

    #[test]
    fn leaves_color_environment_unchanged_without_a_tui() {
        let mut command = Command::new("example");

        configure_captured_output(&mut command, false);

        assert_eq!(command_env(&command, "FORCE_COLOR"), None);
        assert_eq!(command_env(&command, "CLICOLOR_FORCE"), None);
    }

    fn command_env<'a>(command: &'a Command, name: &str) -> Option<&'a OsStr> {
        command.get_envs().find_map(
            |(key, value)| {
                if key == OsStr::new(name) { value } else { None }
            },
        )
    }
}
