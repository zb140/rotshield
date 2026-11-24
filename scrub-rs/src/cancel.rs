use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

static SIGNAL_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn shutdown_signal_handler(_sig: nix::libc::c_int) {
    SIGNAL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install a minimal signal handler before starting any worker threads.
/// No locks, allocations, or filesystem operations run in signal context.
pub fn install_shutdown_signals() -> std::io::Result<()> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(shutdown_signal_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        unsafe { sigaction(sig, &action) }
            .map_err(|e| std::io::Error::other(format!("installing {sig:?}: {e}")))?;
    }
    Ok(())
}

/// Whether an external termination signal has requested shutdown.
pub fn signal_requested() -> bool {
    SIGNAL_REQUESTED.load(Ordering::SeqCst)
}

#[derive(Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || signal_requested()
    }
}
