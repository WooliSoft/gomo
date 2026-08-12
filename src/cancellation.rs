use std::process::{Child, Command, ExitStatus};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Clone, Default, Debug)]
pub(crate) struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub(crate) struct ChildCancellation {
    token: CancellationToken,
    started_at: Option<Instant>,
}

impl ChildCancellation {
    pub(crate) fn new(token: CancellationToken) -> Self {
        Self {
            token,
            started_at: None,
        }
    }

    pub(crate) fn poll(&mut self, child: &mut Child) {
        if !self.token.is_cancelled() {
            return;
        }
        match self.started_at {
            None => {
                terminate_child(child, false);
                self.started_at = Some(Instant::now());
            }
            Some(started_at) if started_at.elapsed() >= Duration::from_millis(250) => {
                terminate_child(child, true);
            }
            Some(_) => {}
        }
    }

    fn force_if_cancelled(&self, child: &mut Child) {
        if self.token.is_cancelled() {
            terminate_child(child, true);
        }
    }
}

pub(crate) fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

pub(crate) fn wait_for_child(
    child: &mut Child,
    cancellation: &CancellationToken,
) -> std::io::Result<ExitStatus> {
    let mut child_cancellation = ChildCancellation::new(cancellation.clone());
    loop {
        child_cancellation.poll(child);
        if let Some(status) = child.try_wait()? {
            child_cancellation.force_if_cancelled(child);
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn terminate_child(child: &mut Child, force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let process_group = -(child.id() as i32);
    // configure_process_group starts the child as this process group's leader.
    unsafe {
        libc::kill(process_group, signal);
    }
}

#[cfg(not(unix))]
fn terminate_child(child: &mut Child, _force: bool) {
    let _ = child.kill();
}
