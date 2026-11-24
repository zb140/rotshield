//! Filesystem freeze control for safe live recovery writes.
//!
//! Recovery writes go to the raw rdev while the filesystem is mounted;
//! freezing it (FIFREEZE) for the duration of one batch's reconfirm+write
//! keeps scrub-rs the only writer to those bytes. Thaw is guaranteed by
//! layers: RAII guard, panic hook, cooperative signal cancellation, and a recorded FITHAW
//! failure that aborts the run with the manual `fsfreeze -u` command.

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use nix::libc;

/// Controls freezing of a (possibly absent) live mounted filesystem during
/// recovery writes.
pub struct FreezeController {
    mountpoint: Option<PathBuf>,
    /// Whether a freeze is currently held; makes thaw() a no-op when not
    /// frozen and stays set after a failed thaw so Drop retries.
    frozen: bool,
    /// Most recent FITHAW failure; read-and-cleared by take_thaw_error.
    last_thaw_error: Option<io::Error>,
}

impl FreezeController {
    /// Build a controller. `None` mountpoint = offline image, never frozen.
    pub fn new(mountpoint: Option<PathBuf>) -> Self {
        Self {
            mountpoint,
            frozen: false,
            last_thaw_error: None,
        }
    }

    /// The declared live mountpoint, if any — for operator-facing messages.
    pub fn mountpoint(&self) -> Option<&Path> {
        self.mountpoint.as_deref()
    }

    /// Whether a live mount was declared (freezing is possible at all).
    pub fn has_live_mount(&self) -> bool {
        self.mountpoint.is_some()
    }

    /// Acquire a scoped freeze. Ok(Some(guard)) = frozen; Ok(None) = no
    /// live mount (nothing to freeze); Err = a live mount was declared but
    /// FIFREEZE failed — the caller must NOT write.
    pub fn guard(&mut self) -> Result<Option<FreezeGuard<'_>>, io::Error> {
        // No live mount declared — nothing to freeze, no freeze needed.
        let Some(mountpoint) = self.mountpoint.as_ref() else {
            return Ok(None);
        };
        match freeze_path(mountpoint) {
            Ok(()) => {
                self.frozen = true;
                note_active_mount(mountpoint);
                install_panic_hook();
                Ok(Some(FreezeGuard { controller: self }))
            }
            Err(e) => Err(e),
        }
    }

    /// Thaw the currently-held freeze, if any. A failed FITHAW is recorded
    /// (the filesystem may still be frozen) and retried on later drops.
    fn thaw(&mut self) {
        if !self.frozen {
            return;
        }
        if let Some(mountpoint) = self.mountpoint.as_ref() {
            match thaw_path(mountpoint) {
                Ok(()) => {
                    clear_active_mount();
                    self.frozen = false;
                }
                Err(e) => {
                    eprintln!(
                        "ERROR: failed to thaw {} after recovery write: {} \
                             (filesystem may still be frozen — run `fsfreeze -u {}`)",
                        mountpoint.display(),
                        e,
                        mountpoint.display(),
                    );
                    self.last_thaw_error = Some(e);
                }
            }
        } else {
            clear_active_mount();
            self.frozen = false;
        }
    }

    /// Return (and clear) the recorded FITHAW failure, if any.
    pub fn take_thaw_error(&mut self) -> Option<io::Error> {
        self.last_thaw_error.take()
    }
}

impl Drop for FreezeController {
    fn drop(&mut self) {
        self.thaw();
    }
}

/// RAII guard: thaws the filesystem when dropped. Held for one batch's
/// reconfirm+write window.
pub struct FreezeGuard<'a> {
    controller: &'a mut FreezeController,
}

impl Drop for FreezeGuard<'_> {
    fn drop(&mut self) {
        self.controller.thaw();
    }
}

/// Best-effort panic hook that thaws any active freeze. Installed once on
/// the first successful freeze so a panic mid-write does not leave the
/// filesystem frozen.
fn install_panic_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(mnt) = ACTIVE_FREEZE_MOUNT.lock().ok().and_then(|g| g.clone()) {
            let _ = thaw_path(Path::new(&mnt));
        }
        prev(info);
    }));
}

/// Tracks the mountpoint of the currently-held freeze so the panic hook
/// and panic hook can thaw it as a last resort.
static ACTIVE_FREEZE_MOUNT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Record the active freeze mountpoint (called by guard() on success).
pub(crate) fn note_active_mount(mnt: &Path) {
    if let Ok(mut g) = ACTIVE_FREEZE_MOUNT.lock() {
        *g = Some(mnt.to_path_buf());
    }
}

/// Clear the active freeze mountpoint (called by thaw()).
pub(crate) fn clear_active_mount() {
    if let Ok(mut g) = ACTIVE_FREEZE_MOUNT.lock() {
        *g = None;
    }
}

/// FIFREEZE / FITHAW ioctl request codes (Linux). Not exposed by the
/// libc crate, so derived from the standard _IOWR('X', n, int) expansion.
const FIFREEZE: std::os::raw::c_ulong = 0xC004_5877;
const FITHAW: std::os::raw::c_ulong = 0xC004_5878;

/// Freeze the filesystem mounted at `mountpoint` via the FIFREEZE ioctl.
fn freeze_path(mountpoint: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new().read(true).open(mountpoint)?;

    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FIFREEZE, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Thaw a previously-frozen filesystem at `mountpoint` via FITHAW.
fn thaw_path(mountpoint: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new().read(true).open(mountpoint)?;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FITHAW, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Thin wrapper over `libc::ioctl` for other modules that need raw ioctls.
pub(crate) unsafe fn libc_ioctl(
    fd: std::os::raw::c_int,
    request: std::os::raw::c_ulong,
    arg: std::os::raw::c_ulong,
) -> std::os::raw::c_int {
    unsafe {
        libc::ioctl(
            fd,
            request as std::os::raw::c_ulong,
            arg as std::os::raw::c_ulong,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_MOUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn failed_thaw_is_recorded_and_keeps_frozen_state() {
        let _guard = TEST_MOUNT_LOCK.lock().unwrap();
        clear_active_mount();

        note_active_mount(Path::new("/nonexistent/freeze-test-mnt"));
        let mut c = FreezeController {
            mountpoint: Some(PathBuf::from("/nonexistent/freeze-test-mnt")),
            frozen: true,
            last_thaw_error: None,
        };
        c.thaw();
        assert!(
            c.frozen,
            "failed thaw must keep frozen so Drop/retry stays possible"
        );
        assert!(
            c.take_thaw_error().is_some(),
            "failed thaw must be observable via take_thaw_error"
        );
        assert!(
            c.take_thaw_error().is_none(),
            "take_thaw_error must clear the recorded error"
        );

        assert!(
            ACTIVE_FREEZE_MOUNT.lock().unwrap().is_some(),
            "active-freeze marker must stay set after a failed thaw"
        );

        // This test deliberately simulates an unrecoverable thaw failure.
        // Clear its synthetic global marker before returning.
        clear_active_mount();
    }

    #[test]
    fn no_thaw_error_by_default_and_thaw_noop_when_not_frozen() {
        let _guard = TEST_MOUNT_LOCK.lock().unwrap();
        clear_active_mount();
        let mut c = FreezeController::new(Some(PathBuf::from("/nonexistent/freeze-test-mnt")));
        assert!(c.take_thaw_error().is_none());
        c.thaw();
        assert!(!c.frozen);
        assert!(c.take_thaw_error().is_none());
    }

    #[test]
    fn no_mount_controller_never_frozen() {
        let mut c = FreezeController::new(None);
        assert!(!c.has_live_mount());
        assert!(c.guard().unwrap().is_none());
        assert!(c.take_thaw_error().is_none());
    }

    #[test]
    #[ignore = "requires mounted disposable Btrfs fixture and root"]
    fn live_freeze_thaw_smoke_test() {
        use std::io::Write;

        let testdir = std::fs::read_to_string("/tmp/scrub-rs-parity-testdir")
            .expect("missing parity fixture pointer");
        let mountpoint = std::path::PathBuf::from(testdir.trim()).join("mnt");

        assert!(mountpoint.is_dir(), "fixture mountpoint does not exist");

        let mut controller = super::FreezeController::new(Some(mountpoint.clone()));

        {
            let guard = controller.guard().expect("FIFREEZE failed");
            assert!(guard.is_some(), "expected an active freeze guard");
        }

        assert!(controller.take_thaw_error().is_none(), "FITHAW failed");

        let probe = mountpoint.join(".freeze-thaw-probe");
        let mut file = std::fs::File::create(&probe).expect("filesystem not writable after thaw");
        file.write_all(b"thaw verified\n")
            .expect("write failed after thaw");
        file.sync_all().expect("sync failed after thaw");
        std::fs::remove_file(&probe).expect("probe cleanup failed");
    }
}
