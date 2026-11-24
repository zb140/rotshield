//! Batched parity recovery: dedup → freeze → re-confirm → recover → write-back.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::freeze::FreezeController;
use crate::fs::{Reconfirm, ReconfirmRequest, Reconfirmer, SectorVerifier};
use crate::recovery::{RecoveryInput, RecoveryResult, recover_block};
use crate::status::StatusCounters;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestFault {
    None,
    Write,
    CacheFlush,
    Readback,
    ReadbackIo,
    Thaw,
}

#[cfg(test)]
thread_local! {
    static TEST_FAULT: std::cell::Cell<TestFault> =
        const { std::cell::Cell::new(TestFault::None) };
}

#[cfg(test)]
struct TestFaultGuard(TestFault);

#[cfg(test)]
impl TestFaultGuard {
    fn new(fault: TestFault) -> Self {
        let previous = TEST_FAULT.with(|current| {
            let previous = current.get();
            current.set(fault);
            previous
        });
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for TestFaultGuard {
    fn drop(&mut self) {
        TEST_FAULT.with(|current| current.set(self.0));
    }
}
/// One mismatched sector queued for recovery.
#[derive(Clone)]
pub struct Candidate {
    pub array_phys: u64,

    pub block_size: usize,

    /// Verifies a candidate block against the sector's stored checksum.
    pub verify: SectorVerifier,

    pub raw_phys: u64,

    pub failing_dev: PathBuf,

    /// Opaque request to re-check the sector against live metadata at write time.
    pub reconfirm: Option<ReconfirmRequest>,

    /// The source read returned EIO; `verify` still carries the stored checksum.
    pub unreadable: bool,
}

/// Messages from the scrub thread to the accumulator.
pub enum Msg {
    /// One mismatched sector.
    Candidate(Candidate),

    /// The scrub finished; flush everything queued.
    /// No more batches will arrive; drain the pending remainder.
    Done,
}

/// Messages from the accumulator to the writer thread.
enum BatchMsg {
    /// A full batch of candidates.
    Batch(Vec<Candidate>),

    Done,
}

/// Recovery accounting; stage counters (dedup/stale/skipped/mismatch) and
/// outcome buckets are separate partitions of the same candidates.
#[derive(Debug, Default)]
pub struct BatchStats {
    /// Candidates that entered recovery (after dedup and re-confirm).
    pub mismatch: u64,

    /// Sectors no longer corrupt (rewritten or freed) — nothing to do.
    pub stale: u64,

    /// Reconstructed, written, and read-back verified (or dry-run).
    pub recovered: u64,

    /// Recovery attempted but not achieved.
    pub failed: u64,

    /// Skipped without recovery (e.g. re-confirm metadata unreadable).
    pub skipped: u64,

    /// Would have been written, but the freeze failed (assess-only batch).
    pub not_frozen: u64,

    /// Written but the read-back could not be verified.
    pub readback_failed: u64,

    /// Failed-disk read matched the stored checksum.
    pub not_corrupt: u64,

    /// Duplicate reports of the same sector collapsed in a batch.
    pub deduped: u64,
}

pub type WriterResult = io::Result<BatchStats>;

/// Start the accumulator and writer threads; returns the send handle for
/// candidates and join handles for both threads. Channels are synchronous
/// (capacity 1) so a slow writer back-pressures the scrub.
#[allow(clippy::too_many_arguments)]
pub fn spawn_pipeline(
    cfg: ArrayConfig,
    freeze: FreezeController,
    reconfirmer: Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    batch_max: usize,
    batch_idle: Duration,
    freeze_timeout: Duration,
    counters: Option<Arc<StatusCounters>>,
    cancel: crate::cancel::CancelToken,
) -> io::Result<(
    SyncSender<Msg>,
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<WriterResult>,
)> {
    let (tx_a, rx_a) = mpsc::sync_channel(1);
    let (tx_b, rx_b) = mpsc::sync_channel(1);

    let acc_handle = std::thread::Builder::new()
        .name("scrub-accum".into())
        .spawn(move || run_accumulator(rx_a, tx_b, batch_max, batch_idle))
        .expect("spawn scrub-accumulator thread");

    let writer_handle = std::thread::Builder::new()
        .name("scrub-writer".into())
        .spawn(move || {
            run_writer(
                rx_b,
                cfg,
                freeze,
                reconfirmer,
                scrub_slot,
                dry_run,
                freeze_timeout,
                counters.as_deref(),
                cancel,
            )
        })
        .expect("spawn scrub-writer thread");

    Ok((tx_a, acc_handle, writer_handle))
}

/// Collect candidates from the scrub and forward them to the writer in
/// batches of up to `batch_max`, flushing early when `batch_idle` elapses.
/// On Done/disconnect, flush whatever remains.
fn run_accumulator(
    rx: mpsc::Receiver<Msg>,
    tx_b: SyncSender<BatchMsg>,
    batch_max: usize,
    batch_idle: Duration,
) {
    let mut batch: Vec<Candidate> = Vec::with_capacity(batch_max);

    loop {
        // Wait for the first candidate; when the scrub finishes, send a
        // final batch plus Done.
        match rx.recv() {
            Ok(Msg::Candidate(c)) => batch.push(c),
            Ok(Msg::Done) => {
                if !batch.is_empty()
                    && tx_b
                        .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                        .is_err()
                {
                    return;
                }
                let _ = tx_b.send(BatchMsg::Done);
                return;
            }
            Err(_) => {
                // The scrub ended without Done. Discard the unfinished batch;
                // an unsuccessful scan must not initiate additional repairs.
                return;
            }
        }

        while batch.len() < batch_max {
            match rx.recv_timeout(batch_idle) {
                Ok(Msg::Candidate(c)) => batch.push(c),
                Ok(Msg::Done) => {
                    if !batch.is_empty()
                        && tx_b
                            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                            .is_err()
                    {
                        return;
                    }
                    let _ = tx_b.send(BatchMsg::Done);
                    return;
                }
                // Batch filled the idle window — send what we have.
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    // No successful completion signal: discard pending candidates.
                    return;
                }
            }
        }

        // Hand the batch to the writer and start the next one (full or idle timeout).
        if tx_b
            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
            .is_err()
        {
            return;
        }
    }
}

/// Receive batches and flush each one; on Done, keep flushing the deferred
/// remainder until nothing is left.
#[allow(clippy::too_many_arguments)]
fn run_writer(
    rx: mpsc::Receiver<BatchMsg>,
    cfg: ArrayConfig,
    mut freeze: FreezeController,
    mut reconfirmer: Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
    cancel: crate::cancel::CancelToken,
) -> io::Result<BatchStats> {
    let mut stats = BatchStats::default();

    let mut pending: Vec<Candidate> = Vec::new();

    loop {
        if cancel.requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "recovery cancelled before starting another batch",
            ));
        }

        match rx.recv() {
            Ok(BatchMsg::Batch(batch)) => {
                if cancel.requested() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "recovery cancelled before processing queued batch",
                    ));
                }

                // Deferred candidates from freeze-timeouts are prepended to the next flush.
                pending.extend(batch);
                if let Err(e) = flush_pending(
                    &mut pending,
                    &cfg,
                    &mut freeze,
                    &mut reconfirmer,
                    scrub_slot,
                    dry_run,
                    freeze_timeout,
                    counters,
                    &mut stats,
                    &cancel,
                ) {
                    cancel.request();
                    eprintln!("error: recovery pipeline aborted: {e}");
                    return Err(e);
                }
            }
            Ok(BatchMsg::Done) => {
                while !pending.is_empty() {
                    if cancel.requested() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "recovery cancelled while draining deferred candidates",
                        ));
                    }

                    if let Err(e) = flush_pending(
                        &mut pending,
                        &cfg,
                        &mut freeze,
                        &mut reconfirmer,
                        scrub_slot,
                        dry_run,
                        freeze_timeout,
                        counters,
                        &mut stats,
                        &cancel,
                    ) {
                        cancel.request();
                        eprintln!("error: recovery pipeline aborted: {e}");
                        return Err(e);
                    }
                }

                if cancel.requested() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "recovery cancelled before completion",
                    ));
                }

                return Ok(stats);
            }

            Err(_) => {
                cancel.request();
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "recovery batch channel disconnected without Done",
                ));
            }
        }
    }
}

/// Flush the pending candidates, keeping the deferred remainder as pending.
#[allow(clippy::too_many_arguments)]
fn flush_pending(
    pending: &mut Vec<Candidate>,
    cfg: &ArrayConfig,
    freeze: &mut FreezeController,
    reconfirmer: &mut Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
    stats: &mut BatchStats,
    cancel: &crate::cancel::CancelToken,
) -> io::Result<()> {
    let deferred = flush_batch(
        pending,
        cfg,
        freeze,
        reconfirmer,
        scrub_slot,
        dry_run,
        freeze_timeout,
        counters,
        stats,
        cancel,
    )?;
    *pending = deferred;
    Ok(())
}

/// Process one batch: sort and dedup by array offset, freeze the filesystem,
/// then per candidate re-confirm → read the failing disk → gather the stripe →
/// recover → write back → read-back verify. Returns the candidates deferred
/// by the freeze-timeout; aborts on a failed thaw.
#[allow(clippy::too_many_arguments)]
fn flush_batch(
    batch: &[Candidate],
    cfg: &ArrayConfig,
    freeze: &mut FreezeController,
    reconfirmer: &mut Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
    stats: &mut BatchStats,
    cancel: &crate::cancel::CancelToken,
) -> io::Result<Vec<Candidate>> {
    let mut batch = batch.to_vec();
    // Dedup: the scrub can report the same sector more than once.
    batch.sort_by_key(|a| a.array_phys);
    let pre_dedup = batch.len();
    batch.dedup_by(|a, b| a.array_phys == b.array_phys);
    let deduped = (pre_dedup - batch.len()) as u64;
    stats.deduped += deduped;
    if let Some(c) = counters {
        c.deduped.fetch_add(deduped, Ordering::Relaxed);
    }

    // One freeze covers the whole batch. A failed freeze makes the batch
    // assess-only: candidates are classified but never written.
    let has_live_mount = freeze.has_live_mount();
    let freeze_guard = match freeze.guard() {
        Ok(g) => g,
        // Freeze failed: the filesystem is not frozen, so this batch runs
        // assess-only — candidates are classified but never written.
        Err(e) => {
            eprintln!(
                "ERROR: could not freeze the live filesystem for this batch ({e}). \
                 The filesystem is NOT frozen, so NO writes will be issued for this batch \
                 (assess-only; would-be writes counted as not_frozen). Later batches retry \
                 the freeze — nothing is ever written unfrozen unless --no-freeze is set \
                 explicitly."
            );
            None
        }
    };

    // Writes proceed only under an active freeze (or when there is no live
    // mount at all, e.g. an offline image).
    let writes_allowed = freeze_guard.is_some() || !has_live_mount;

    // Bound the freeze window: once exceeded, defer the rest of the batch
    // (still classified exactly once) to the next flush.
    let freeze_started = std::time::Instant::now();
    let mut deferred: Vec<Candidate> = Vec::new();

    // Remember the first fatal write/verification error, but do not return
    // until the filesystem has been thawed.
    let mut fatal_write_error: Option<io::Error> = None;

    for (i, cand) in batch.iter().enumerate() {
        if cancel.requested() {
            fatal_write_error = Some(io::Error::new(
                io::ErrorKind::Interrupted,
                "recovery cancelled during active batch",
            ));
            break;
        }
        if i > 0 && writes_allowed && freeze_started.elapsed() > freeze_timeout {
            deferred.extend_from_slice(&batch[i..]);
            eprintln!(
                "[freeze-timeout] batch window exceeded {}s — deferring {} candidate(s) to \
                 the next batch{}",
                freeze_timeout.as_secs(),
                batch.len() - i,
                if freeze_guard.is_some() {
                    " (filesystem thawed)"
                } else {
                    ""
                }
            );
            break;
        }

        // Re-confirm against live metadata: the sector may have been
        // rewritten or freed since the scan.
        let verdict = match &cand.reconfirm {
            Some(req) => reconfirmer.reconfirm(req),

            None => Reconfirm::Corruption,
        };

        match verdict {
            Reconfirm::Stale => {
                stats.stale += 1;
                if let Some(c) = counters {
                    c.stale.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            // Metadata unreadable — cannot safely write; skip this sector.
            Reconfirm::Unverifiable => {
                stats.skipped += 1;
                if let Some(c) = counters {
                    c.skipped.fetch_add(1, Ordering::Relaxed);
                }
                eprintln!(
                    "  [0x{:x}] SKIPPED (re-confirm metadata unreadable for this sector)",
                    cand.raw_phys
                );
                continue;
            }
            Reconfirm::Corruption => {}
        }

        // Re-read the failing disk. If it now verifies, the sector was
        // rewritten since the scan — stale, nothing to recover.
        let mut unreadable = cand.unreadable;
        let corrupt_block: Vec<u8> = if unreadable {
            vec![0u8; cand.block_size]
        } else {
            match stripe::read_block_or_zeros(
                cfg,
                &cand.failing_dev,
                cand.array_phys,
                cand.block_size,
            ) {
                Ok(block) => {
                    if (cand.verify)(&block) {
                        stats.stale += 1;
                        if let Some(c) = counters {
                            c.stale.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                    block
                }
                Err(e) => {
                    eprintln!(
                        "  [0x{:x}] read failing disk: {e} — disk degraded mid-run; \
                         degrading to the unreadable recovery path",
                        cand.raw_phys
                    );
                    unreadable = true;
                    vec![0u8; cand.block_size]
                }
            }
        };
        stats.mismatch += 1;
        if let Some(c) = counters {
            c.mismatch.fetch_add(1, Ordering::Relaxed);
        }

        // Gather the other data disks plus P/Q at this offset.
        let stripe_chunks =
            match stripe::gather_stripe(cfg, scrub_slot, cand.array_phys, cand.block_size) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  [0x{:x}] gather stripe failed: {e}", cand.raw_phys);
                    stats.failed += 1;
                    if let Some(c) = counters {
                        c.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            };
        let input = RecoveryInput {
            failing_slot: scrub_slot,
            corrupt_block: &corrupt_block,
            unreadable_source: unreadable,
            other_blocks: &stripe_chunks.other_data,
            p_block: stripe_chunks.p_block.as_deref(),
            q_block: stripe_chunks.q_block.as_deref(),
            verifier: &*cand.verify,
        };
        // Try P, then Q, then the PQ 2-disk solve; the verifier picks the
        // first candidate that matches the stored checksum.
        let result = recover_block(&input, cand.block_size);
        match &result {
            RecoveryResult::Recovered { via, block } => {
                let via_str = match via {
                    crate::recovery::ParityPath::P => "P".to_string(),
                    crate::recovery::ParityPath::Q => "Q".to_string(),
                    crate::recovery::ParityPath::PQ { partner_slot } => {
                        format!("PQ(partner=slot {partner_slot})")
                    }
                };

                // Dry-run or assess-only: report the would-be write, never issue it.
                if dry_run || !writes_allowed {
                    if writes_allowed {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} (dry-run) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.recovered += 1;
                        if let Some(c) = counters {
                            c.recovered.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} but NOT WRITTEN (batch had no \
                             freeze — deferred; assess-only) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.not_frozen += 1;
                        if let Some(c) = counters {
                            c.not_frozen.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    continue;
                }

                if cancel.requested() {
                    fatal_write_error = Some(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "recovery cancelled before repair write",
                    ));
                    break;
                }

                // Write the recovered block straight to the raw rdev.
                #[cfg(test)]
                let write_result = if TEST_FAULT.with(|fault| fault.get() == TestFault::Write) {
                    Err(io::Error::other("injected write failure"))
                } else {
                    stripe::write_block(cfg, &cand.failing_dev, cand.array_phys, block)
                };

                #[cfg(not(test))]
                let write_result =
                    stripe::write_block(cfg, &cand.failing_dev, cand.array_phys, block);

                match write_result {
                    Ok(()) => {
                        if let Some(c) = counters
                            && has_live_mount
                        {
                            c.repaired_while_mounted.store(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("  [0x{:x}] write back failed: {e}", cand.raw_phys);
                        stats.failed += 1;
                        if let Some(c) = counters {
                            c.failed.fetch_add(1, Ordering::Relaxed);
                        }
                        fatal_write_error = Some(io::Error::other(format!(
                            "write back failed at raw offset 0x{:x}: {e}",
                            cand.raw_phys
                        )));
                        break;
                    }
                }

                // Drop the device page cache so the read-back reads the platter,
                // not stale cache.
                #[cfg(test)]
                let cache_result = if TEST_FAULT.with(|fault| fault.get() == TestFault::CacheFlush)
                {
                    Err(io::Error::other("injected cache invalidation failure"))
                } else {
                    drop_cache(&cand.failing_dev)
                };

                #[cfg(not(test))]
                let cache_result = drop_cache(&cand.failing_dev);

                match cache_result {
                    Ok(()) => {}
                    Err(e) => {
                        use std::os::unix::fs::FileTypeExt;
                        let is_block = std::fs::metadata(&cand.failing_dev)
                            .map(|m| m.file_type().is_block_device())
                            .unwrap_or(false);
                        if is_block || {
                            #[cfg(test)]
                            {
                                TEST_FAULT.with(|fault| fault.get() == TestFault::CacheFlush)
                            }
                            #[cfg(not(test))]
                            {
                                false
                            }
                        } {
                            eprintln!(
                                "  [0x{:x}] READ-BACK VERIFY NOT TRUSTED: could not \
                                 invalidate the raw rdev cache before read-back ({e}) — the \
                                 verify would read the page cache, not the platter. Counted \
                                 FAILED (readback_failed) instead of verified.",
                                cand.raw_phys
                            );
                            stats.readback_failed += 1;
                            if let Some(c) = counters {
                                c.readback_failed.fetch_add(1, Ordering::Relaxed);
                            }

                            fatal_write_error = Some(io::Error::other(format!(
                                "cache invalidation failed after write at raw offset 0x{:x}: {e}",
                                cand.raw_phys
                            )));
                            break;
                        }
                        eprintln!(
                            "  [0x{:x}] note: BLKFLSBUF unavailable on this image ({e}); \
                             read-back reads the image's page cache, which is authoritative \
                             after fsync",
                            cand.raw_phys
                        );
                    }
                }
                // Read-back verification: only count the write as recovered when
                // the device returns bytes that verify.
                #[cfg(test)]
                let reread = match TEST_FAULT.with(|fault| fault.get()) {
                    TestFault::Readback => Ok(vec![0u8; cand.block_size]),
                    TestFault::ReadbackIo => {
                        Err(io::Error::other("injected read-back I/O failure"))
                    }
                    _ => stripe::read_block_or_zeros(
                        cfg,
                        &cand.failing_dev,
                        cand.array_phys,
                        cand.block_size,
                    ),
                };

                #[cfg(not(test))]
                let reread = stripe::read_block_or_zeros(
                    cfg,
                    &cand.failing_dev,
                    cand.array_phys,
                    cand.block_size,
                );
                match reread {
                    Ok(b) if (cand.verify)(&b) => {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} (written + read-back verified) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.recovered += 1;
                        if let Some(c) = counters {
                            c.recovered.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {
                        eprintln!(
                            "  [0x{:x}] READ-BACK MISMATCH: device returned bytes that fail \
                             the verifier after a successful write (lying/failing disk?) — \
                             counted FAILED (readback_failed)",
                            cand.raw_phys
                        );
                        stats.readback_failed += 1;
                        if let Some(c) = counters {
                            c.readback_failed.fetch_add(1, Ordering::Relaxed);
                        }

                        fatal_write_error = Some(io::Error::other(format!(
                            "read-back verification mismatch at raw offset 0x{:x}",
                            cand.raw_phys
                        )));
                        break;
                    }
                    Err(e) => {
                        eprintln!(
                            "  [0x{:x}] read-back error after write: {e} — counted FAILED \
                             (readback_failed)",
                            cand.raw_phys
                        );
                        stats.readback_failed += 1;
                        if let Some(c) = counters {
                            c.readback_failed.fetch_add(1, Ordering::Relaxed);
                        }
                        fatal_write_error = Some(io::Error::other(format!(
                            "read-back error at raw offset 0x{:x}: {e}",
                            cand.raw_phys
                        )));
                        break;
                    }
                }

                let _ = drop_cache(&cand.failing_dev);
            }
            // The on-disk block already matches its checksum (raced rewrite).
            RecoveryResult::NotCorrupt => {
                eprintln!(
                    "  [0x{:x}] not corrupt (matches stored csum)",
                    cand.raw_phys
                );
                stats.not_corrupt += 1;
                if let Some(c) = counters {
                    c.not_corrupt.fetch_add(1, Ordering::Relaxed);
                }
            }
            // All parity paths failed or the verifier rejected everything.
            RecoveryResult::Failed { reason } => {
                eprintln!(
                    "  [0x{:x}] FAILED: {reason:?} dev={}",
                    cand.raw_phys,
                    cand.failing_dev.display(),
                );
                stats.failed += 1;
                if let Some(c) = counters {
                    c.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // A failed thaw means the filesystem may still be frozen — abort the run
    // rather than risk unfrozen writes in later batches.
    drop(freeze_guard);

    #[cfg(test)]
    let thaw_error = if TEST_FAULT.with(|fault| fault.get() == TestFault::Thaw) {
        Some(io::Error::other("injected thaw failure"))
    } else {
        freeze.take_thaw_error()
    };

    #[cfg(not(test))]
    let thaw_error = freeze.take_thaw_error();

    if let Some(e) = thaw_error {
        if let Some(counters) = counters {
            counters.record_thaw_failure();
        }
        let mnt = freeze
            .mountpoint()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<--freeze-mount path>".to_string());
        return Err(io::Error::other(format!(
            "failed to thaw the live filesystem after this batch ({e}) — it may STILL \
             BE FROZEN. Thaw it manually with `fsfreeze -u {mnt}`, then re-run. Aborting \
             the run: no further batches will start while the filesystem may be frozen."
        )));
    }
    if let Some(e) = fatal_write_error {
        return Err(e);
    }
    Ok(deferred)
}

/// Invalidate the kernel's page cache for the device (BLKFLSBUF ioctl) so
/// subsequent reads come from the platter.
fn drop_cache(dev: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(dev)?;

    const BLKFLSBUF: std::os::raw::c_ulong = 0x0000_1261;
    let rc = unsafe { crate::freeze::libc_ioctl(f.as_raw_fd(), BLKFLSBUF, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::config::ArrayConfig;
    use crate::fs::{Reconfirm, ReconfirmRequest};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    const BLOCK: usize = 16;

    struct VerdictStub {
        stale_tokens: Vec<u64>,
        unverifiable_tokens: Vec<u64>,
    }
    impl Reconfirmer for VerdictStub {
        fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm {
            if self.stale_tokens.contains(&req.token) {
                Reconfirm::Stale
            } else if self.unverifiable_tokens.contains(&req.token) {
                Reconfirm::Unverifiable
            } else {
                Reconfirm::Corruption
            }
        }
    }

    struct CancellingReconfirmer {
        cancel: crate::cancel::CancelToken,
    }

    impl Reconfirmer for CancellingReconfirmer {
        fn reconfirm(&mut self, _req: &ReconfirmRequest) -> Reconfirm {
            self.cancel.request();
            Reconfirm::Corruption
        }
    }

    fn make_disk(dir: &tempfile::TempDir, name: &str, fill: u8) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&[fill; BLOCK * 8]).unwrap();
        f.sync_all().unwrap();
        path
    }

    fn cfg(dir: &tempfile::TempDir) -> (ArrayConfig, PathBuf) {
        let d1 = make_disk(dir, "d1", 0x11);
        let d2 = make_disk(dir, "d2", 0x22);
        let p = make_disk(dir, "p", 0x66);
        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([(1, d1.clone()), (2, d2)]),
            parity_p: Some(p),
            parity_q: None,
            rdev_offsets: BTreeMap::new(),
        };
        (cfg, d1)
    }

    fn cand(failing_dev: PathBuf, array_phys: u64, token: u64, unreadable: bool) -> Candidate {
        let verify: SectorVerifier = Arc::new(|b: &[u8]| b == &vec![0x44u8; BLOCK][..]);
        Candidate {
            array_phys,
            block_size: BLOCK,
            verify,
            raw_phys: array_phys,
            failing_dev,
            reconfirm: Some(ReconfirmRequest {
                token,
                stored_csum: vec![],
            }),
            unreadable,
        }
    }

    fn run_batch(
        cfg: &ArrayConfig,
        freeze: FreezeController,
        dry_run: bool,
        cands: Vec<Candidate>,
        stub: VerdictStub,
    ) -> BatchStats {
        run_batch_for_slot(cfg, freeze, 1, dry_run, cands, stub)
    }

    fn run_batch_for_slot(
        cfg: &ArrayConfig,
        freeze: FreezeController,
        scrub_slot: u64,
        dry_run: bool,
        cands: Vec<Candidate>,
        stub: VerdictStub,
    ) -> BatchStats {
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(stub);
        let mut stats = BatchStats::default();
        let mut freeze = freeze;

        let deferred = flush_batch(
            &cands,
            cfg,
            &mut freeze,
            &mut reconfirmer,
            scrub_slot,
            dry_run,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        )
        .expect("batch must not abort");

        assert!(
            deferred.is_empty(),
            "no freeze-timeout deferral expected in this test"
        );

        stats
    }

    #[test]
    fn freeze_failure_defers_writes_into_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();
        let freeze = FreezeController::new(Some(PathBuf::from("/nonexistent/mountpoint")));
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.not_frozen, 1, "write must be deferred");
        assert_eq!(stats.recovered, 0, "deferred writes are never recovered");
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.mismatch, 1);
        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "assess-only batch must not touch the disk"
        );
    }

    #[test]
    fn no_declared_mount_writes_normally() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.not_frozen, 0);
        let after = fs::read(&d1).unwrap();
        assert_eq!(
            &after[..BLOCK],
            &vec![0x44u8; BLOCK],
            "block must be repaired on the failing disk"
        );
    }

    #[test]
    fn dry_run_counts_recovered_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            true,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1, "dry-run reports would-be recovery");
        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "dry-run must not touch the disk"
        );
    }

    #[test]
    fn unreadable_candidate_recovers_from_parity() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, true)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1);
        assert_eq!(
            &fs::read(&d1).unwrap()[..BLOCK],
            &vec![0x44u8; BLOCK],
            "unreadable sector must be rebuilt from parity"
        );
    }

    #[test]
    fn verdicts_stale_and_unverifiable_are_counted_and_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![
                cand(d1.clone(), 0, 0, false),
                cand(d1.clone(), 16, 1, false),
                cand(d1.clone(), 32, 2, false),
                cand(d1.clone(), 48, 3, false),
            ],
            VerdictStub {
                stale_tokens: vec![2],
                unverifiable_tokens: vec![3],
            },
        );
        assert_eq!(stats.recovered, 2);
        assert_eq!(stats.stale, 1);
        assert_eq!(stats.skipped, 1);

        assert_eq!(stats.mismatch, 2);
        assert_eq!(
            stats.mismatch,
            stats.recovered
                + stats.failed
                + stats.not_corrupt
                + stats.not_frozen
                + stats.readback_failed
        );
        assert_eq!(
            stats.stale + stats.skipped + stats.mismatch + stats.deduped,
            4
        );

        let after = fs::read(&d1).unwrap();
        assert_eq!(&after[..16], &vec![0x44u8; 16]);
        assert_eq!(&after[16..32], &vec![0x44u8; 16]);
        assert_eq!(&after[32..48], &vec![0x11u8; 16], "stale: not written");
        assert_eq!(&after[48..64], &vec![0x11u8; 16], "skipped: not written");
    }

    #[test]
    fn array_phys_dedup_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false), cand(d1.clone(), 0, 1, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.deduped, 1);
        assert_eq!(stats.mismatch, 1);
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.deduped + stats.mismatch, 2, "sent must reconcile");
    }

    #[test]
    fn write_failure_aborts_batch_without_writing_later_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);

        let before = fs::read(&d1).unwrap();

        let candidates = vec![
            cand(d1.clone(), 0, 0, false),
            cand(d1.clone(), 16, 1, false),
        ];

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let _fault = TestFaultGuard::new(TestFault::Write);

        let result = flush_batch(
            &candidates,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("injected write failure must abort the batch"),
        };
        assert!(
            err.to_string().contains("injected write failure"),
            "unexpected error: {err}"
        );

        assert_eq!(stats.mismatch, 1, "only first candidate was processed");
        assert_eq!(stats.failed, 1, "failed write was counted");
        assert_eq!(stats.recovered, 0);

        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "no candidate should have been written"
        );
    }

    #[test]
    fn readback_mismatch_aborts_batch_without_writing_later_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);

        let before = fs::read(&d1).unwrap();

        let candidates = vec![
            cand(d1.clone(), 0, 0, false),
            cand(d1.clone(), 16, 1, false),
        ];

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let _fault = TestFaultGuard::new(TestFault::Readback);

        let result = flush_batch(
            &candidates,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("read-back mismatch must abort the batch"),
        };

        assert!(
            err.to_string().contains("read-back verification mismatch"),
            "unexpected error: {err}"
        );

        assert_eq!(stats.mismatch, 1, "only first candidate was processed");
        assert_eq!(stats.readback_failed, 1);
        assert_eq!(stats.recovered, 0);

        let after = fs::read(&d1).unwrap();

        assert_eq!(
            &after[..16],
            &vec![0x44u8; 16],
            "first candidate was written before read-back verification failed"
        );
        assert_eq!(
            &after[16..],
            &before[16..],
            "second candidate must remain untouched"
        );
    }

    #[test]
    fn readback_io_failure_aborts_batch_without_writing_later_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);

        let before = fs::read(&d1).unwrap();

        let candidates = vec![
            cand(d1.clone(), 0, 0, false),
            cand(d1.clone(), 16, 1, false),
        ];

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let _fault = TestFaultGuard::new(TestFault::ReadbackIo);

        let result = flush_batch(
            &candidates,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("read-back I/O failure must abort the batch"),
        };

        assert!(err.to_string().contains(""), "unexpected error: {err}");

        assert_eq!(stats.mismatch, 1, "only first candidate was processed");
        assert_eq!(stats.readback_failed, 1);
        assert_eq!(stats.recovered, 0);

        let after = fs::read(&d1).unwrap();

        assert_eq!(
            &after[..16],
            &vec![0x44u8; 16],
            "first candidate was written before read-back verification failed"
        );
        assert_eq!(
            &after[16..],
            &before[16..],
            "second candidate must remain untouched"
        );
    }
    #[test]
    fn cache_failure_aborts_batch_without_writing_later_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);

        let before = fs::read(&d1).unwrap();

        let candidates = vec![
            cand(d1.clone(), 0, 0, false),
            cand(d1.clone(), 16, 1, false),
        ];

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let _fault = TestFaultGuard::new(TestFault::CacheFlush);

        let result = flush_batch(
            &candidates,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("injected cache failure must abort the batch"),
        };

        assert!(
            err.to_string()
                .contains("injected cache invalidation failure"),
            "unexpected error: {err}"
        );

        assert_eq!(stats.mismatch, 1, "only first candidate was processed");
        assert_eq!(stats.readback_failed, 1);
        assert_eq!(stats.recovered, 0);

        let after = fs::read(&d1).unwrap();

        assert_eq!(
            &after[..16],
            &vec![0x44u8; 16],
            "first candidate was written before cache invalidation failed"
        );
        assert_eq!(
            &after[16..],
            &before[16..],
            "later candidates must remain untouched"
        );
    }

    #[test]
    fn fresh_read_failure_degrades_to_unreadable_recovery_path() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        fs::remove_file(&d1).unwrap();
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            true,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(
            stats.recovered, 1,
            "degraded candidate must be recovered from parity"
        );
        assert_eq!(
            stats.failed, 0,
            "fresh-read failure must not auto-fail the candidate"
        );
        assert_eq!(stats.mismatch, 1);
    }

    #[test]
    fn freeze_timeout_defers_the_rest_of_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();
        let mut freeze = FreezeController::new(None);

        let deferred = flush_batch(
            &[
                cand(d1.clone(), 0, 0, false),
                cand(d1.clone(), 16, 1, false),
                cand(d1.clone(), 32, 2, false),
            ],
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        )
        .expect("deferral is not an abort");
        assert_eq!(deferred.len(), 2, "candidates 2 and 3 deferred");
        assert_eq!(stats.recovered, 1, "first candidate still classified");
        assert_eq!(stats.mismatch, 1);
        assert_eq!(
            &fs::read(&d1).unwrap()[..16],
            &vec![0x44u8; 16],
            "first candidate was written before the timeout"
        );
        assert_eq!(
            &fs::read(&d1).unwrap()[16..48],
            &vec![0x11u8; 32],
            "deferred candidates not written yet"
        );

        let mut stats2 = BatchStats::default();
        let deferred2 = flush_batch(
            &deferred,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats2,
            &crate::cancel::CancelToken::new(),
        )
        .expect("second flush must not abort");
        assert_eq!(deferred2.len(), 1);
        assert_eq!(stats2.recovered, 1);
        let deferred3 = flush_batch(
            &deferred2,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats2,
            &crate::cancel::CancelToken::new(),
        )
        .expect("third flush must not abort");
        assert!(deferred3.is_empty());
        assert_eq!(stats2.recovered, 2);

        assert_eq!(stats.recovered + stats2.recovered, 3);
    }

    #[test]
    fn integration_4k_p_recovery_repairs_corrupted_sector() {
        const SIZE: usize = 4096;

        let dir = tempfile::tempdir().unwrap();

        // Three data disks containing distinct deterministic data.
        let original: Vec<Vec<u8>> = (1..=3u8)
            .map(|disk| {
                (0..SIZE)
                    .map(|i| (i as u8).wrapping_mul(17).wrapping_add(disk))
                    .collect()
            })
            .collect();

        let mut paths = Vec::new();
        for (i, data) in original.iter().enumerate() {
            let path = dir.path().join(format!("disk{}", i + 1));
            fs::write(&path, data).unwrap();
            paths.push(path);
        }

        // Compute correct P parity from the original data.
        let mut parity = vec![0u8; SIZE];
        for data in &original {
            for (p, d) in parity.iter_mut().zip(data) {
                *p ^= *d;
            }
        }

        let parity_path = dir.path().join("parity_p");
        fs::write(&parity_path, parity).unwrap();

        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([
                (1, paths[0].clone()),
                (2, paths[1].clone()),
                (3, paths[2].clone()),
            ]),
            parity_p: Some(parity_path),
            parity_q: None,
            rdev_offsets: BTreeMap::new(),
        };

        // Corrupt disk 2 directly, leaving parity unchanged.
        let mut damaged = original[0].clone();
        damaged[1234] ^= 0x5a;
        fs::write(&paths[0], &damaged).unwrap();

        assert_ne!(fs::read(&paths[0]).unwrap(), original[1]);

        // Verify against the original bytes.
        let golden = original[0].clone();
        let verify: SectorVerifier = Arc::new(move |block: &[u8]| block == golden.as_slice());

        let candidate = Candidate {
            array_phys: 0,
            block_size: SIZE,
            verify,
            raw_phys: 0,
            failing_dev: paths[0].clone(),
            reconfirm: None,
            unreadable: false,
        };

        let stats = run_batch(
            &cfg,
            FreezeController::new(None),
            false,
            vec![candidate],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );

        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.readback_failed, 0);

        // Most important assertion: the repaired bytes are exactly correct.
        assert_eq!(fs::read(&paths[0]).unwrap(), original[0]);

        // Other data disks must remain untouched.
        assert_eq!(fs::read(&paths[1]).unwrap(), original[1]);
        assert_eq!(fs::read(&paths[2]).unwrap(), original[2]);
    }

    #[test]
    fn integration_4k_q_recovery_when_p_baked_in() {
        const SIZE: usize = 4096;

        let dir = tempfile::tempdir().unwrap();

        // Create three simulated data disks with deterministic contents.
        let original: Vec<Vec<u8>> = (1..=3u8)
            .map(|disk| {
                (0..SIZE)
                    .map(|i| (i as u8).wrapping_mul(17).wrapping_add(disk))
                    .collect()
            })
            .collect();

        let mut paths = Vec::new();

        for (i, data) in original.iter().enumerate() {
            let path = dir.path().join(format!("disk{}", i + 1));
            fs::write(&path, data).unwrap();
            paths.push(path);
        }

        let parity_p_path = dir.path().join("parity_p");
        let parity_q_path = dir.path().join("parity_q");

        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([
                (1, paths[0].clone()),
                (2, paths[1].clone()),
                (3, paths[2].clone()),
            ]),
            parity_p: Some(parity_p_path.clone()),
            parity_q: Some(parity_q_path.clone()),
            rdev_offsets: BTreeMap::new(),
        };

        // Generate Q from the original, uncorrupted data.
        let q_original = crate::array::parity::compute_q(&cfg, 0, SIZE)
            .expect("Q parity calculation must succeed");

        fs::write(&parity_q_path, &q_original).unwrap();

        // Corrupt data disk 2.
        let mut damaged = original[1].clone();
        damaged[1234] ^= 0x5a;

        fs::write(&paths[1], &damaged).unwrap();

        // Recompute P from the now-corrupted data.
        // This simulates corruption that has been incorporated into P parity.
        let mut p_baked = vec![0u8; SIZE];

        for path in &paths {
            let data = fs::read(path).unwrap();

            for (p, d) in p_baked.iter_mut().zip(data.iter()) {
                *p ^= *d;
            }
        }

        fs::write(&parity_p_path, &p_baked).unwrap();

        // P reconstruction will produce the corrupted bytes.
        // Q reconstruction should produce the original bytes.
        let golden = original[1].clone();

        let verify: SectorVerifier = Arc::new(move |block: &[u8]| block == golden.as_slice());

        let candidate = Candidate {
            array_phys: 0,
            block_size: SIZE,
            verify,
            raw_phys: 0,
            failing_dev: paths[1].clone(),
            reconfirm: None,
            unreadable: false,
        };

        let stats = run_batch_for_slot(
            &cfg,
            FreezeController::new(None),
            2,
            false,
            vec![candidate],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );

        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.readback_failed, 0);

        // The damaged disk must be restored byte-for-byte.
        assert_eq!(fs::read(&paths[1]).unwrap(), original[1]);

        // All other disks and both parity files must remain unchanged.
        assert_eq!(fs::read(&paths[0]).unwrap(), original[0]);
        assert_eq!(fs::read(&paths[2]).unwrap(), original[2]);
        assert_eq!(fs::read(&parity_p_path).unwrap(), p_baked);
        assert_eq!(fs::read(&parity_q_path).unwrap(), q_original);
    }

    #[test]
    fn integration_4k_unrecoverable_corruption_never_writes() {
        const SIZE: usize = 4096;

        let dir = tempfile::tempdir().unwrap();

        // Create three simulated data disks with deterministic contents.
        let original: Vec<Vec<u8>> = (1..=3u8)
            .map(|disk| {
                (0..SIZE)
                    .map(|i| (i as u8).wrapping_mul(17).wrapping_add(disk))
                    .collect()
            })
            .collect();

        let mut paths = Vec::new();

        for (i, data) in original.iter().enumerate() {
            let path = dir.path().join(format!("disk{}", i + 1));
            fs::write(&path, data).unwrap();
            paths.push(path);
        }

        let parity_p_path = dir.path().join("parity_p");
        let parity_q_path = dir.path().join("parity_q");

        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([
                (1, paths[0].clone()),
                (2, paths[1].clone()),
                (3, paths[2].clone()),
            ]),
            parity_p: Some(parity_p_path.clone()),
            parity_q: Some(parity_q_path.clone()),
            rdev_offsets: BTreeMap::new(),
        };

        // Generate Q from the original, uncorrupted data.
        let q_original = crate::array::parity::compute_q(&cfg, 0, SIZE)
            .expect("Q parity calculation must succeed");

        fs::write(&parity_q_path, &q_original).unwrap();

        // Corrupt data disk 2.
        let mut damaged = original[1].clone();
        damaged[1234] ^= 0x5a;

        fs::write(&paths[1], &damaged).unwrap();

        // Recompute P from the now-corrupted data.
        // This simulates corruption that has been incorporated into P parity.
        let mut p_baked = vec![0u8; SIZE];

        for path in &paths {
            let data = fs::read(path).unwrap();

            for (p, d) in p_baked.iter_mut().zip(data.iter()) {
                *p ^= *d;
            }
        }

        fs::write(&parity_p_path, &p_baked).unwrap();

        // Both parity disks now reflect the corrupted data.
        let q_baked = crate::array::parity::compute_q(&cfg, 0, SIZE)
            .expect("Q parity calculation must succeed");

        fs::write(&parity_q_path, &q_baked).unwrap();

        // P reconstruction will produce the corrupted bytes.
        // Q reconstruction should produce the original bytes.
        let golden = original[1].clone();

        let verify: SectorVerifier = Arc::new(move |block: &[u8]| block == golden.as_slice());

        let candidate = Candidate {
            array_phys: 0,
            block_size: SIZE,
            verify,
            raw_phys: 0,
            failing_dev: paths[1].clone(),
            reconfirm: None,
            unreadable: false,
        };

        let stats = run_batch_for_slot(
            &cfg,
            FreezeController::new(None),
            2,
            false,
            vec![candidate],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );

        assert_eq!(stats.recovered, 0);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.readback_failed, 0);

        // Recovery failed, so the corrupted bytes must remain untouched.
        assert_eq!(fs::read(&paths[1]).unwrap(), damaged);

        // Other data disks must remain unchanged.
        assert_eq!(fs::read(&paths[0]).unwrap(), original[0]);
        assert_eq!(fs::read(&paths[2]).unwrap(), original[2]);

        // Neither parity disk may be modified.
        assert_eq!(fs::read(&parity_p_path).unwrap(), p_baked);
        assert_eq!(fs::read(&parity_q_path).unwrap(), q_baked);
    }

    #[test]
    fn integration_4k_dry_run_recovers_without_writing() {
        const SIZE: usize = 4096;

        let dir = tempfile::tempdir().unwrap();

        // Three data disks containing distinct deterministic data.
        let original: Vec<Vec<u8>> = (1..=3u8)
            .map(|disk| {
                (0..SIZE)
                    .map(|i| (i as u8).wrapping_mul(17).wrapping_add(disk))
                    .collect()
            })
            .collect();

        let mut paths = Vec::new();
        for (i, data) in original.iter().enumerate() {
            let path = dir.path().join(format!("disk{}", i + 1));
            fs::write(&path, data).unwrap();
            paths.push(path);
        }

        // Compute correct P parity from the original data.
        let mut parity = vec![0u8; SIZE];
        for data in &original {
            for (p, d) in parity.iter_mut().zip(data) {
                *p ^= *d;
            }
        }

        let parity_path = dir.path().join("parity_p");
        fs::write(&parity_path, &parity).unwrap();

        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([
                (1, paths[0].clone()),
                (2, paths[1].clone()),
                (3, paths[2].clone()),
            ]),
            parity_p: Some(parity_path.clone()),
            parity_q: None,
            rdev_offsets: BTreeMap::new(),
        };

        // Corrupt disk 2 directly, leaving parity unchanged.
        let mut damaged = original[0].clone();
        damaged[1234] ^= 0x5a;
        fs::write(&paths[0], &damaged).unwrap();

        assert_ne!(fs::read(&paths[0]).unwrap(), original[1]);

        // Verify against the original bytes.
        let golden = original[0].clone();
        let verify: SectorVerifier = Arc::new(move |block: &[u8]| block == golden.as_slice());

        let candidate = Candidate {
            array_phys: 0,
            block_size: SIZE,
            verify,
            raw_phys: 0,
            failing_dev: paths[0].clone(),
            reconfirm: None,
            unreadable: false,
        };

        let stats = run_batch(
            &cfg,
            FreezeController::new(None),
            true,
            vec![candidate],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );

        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.readback_failed, 0);

        // Most important assertion: the repaired bytes are exactly correct.
        assert_eq!(fs::read(&paths[0]).unwrap(), damaged);

        // Other data disks must remain untouched.
        assert_eq!(fs::read(&paths[1]).unwrap(), original[1]);
        assert_eq!(fs::read(&paths[2]).unwrap(), original[2]);

        assert_eq!(fs::read(&parity_path).unwrap(), parity);
    }

    #[test]
    fn integration_4k_two_disk_corruption_recovers_via_pq() {
        const SIZE: usize = 4096;

        let dir = tempfile::tempdir().unwrap();

        // Create three simulated data disks with deterministic contents.
        let original: Vec<Vec<u8>> = (1..=3u8)
            .map(|disk| {
                (0..SIZE)
                    .map(|i| (i as u8).wrapping_mul(17).wrapping_add(disk))
                    .collect()
            })
            .collect();

        let mut paths = Vec::new();

        for (i, data) in original.iter().enumerate() {
            let path = dir.path().join(format!("disk{}", i + 1));
            fs::write(&path, data).unwrap();
            paths.push(path);
        }

        let parity_p_path = dir.path().join("parity_p");
        let parity_q_path = dir.path().join("parity_q");

        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([
                (1, paths[0].clone()),
                (2, paths[1].clone()),
                (3, paths[2].clone()),
            ]),
            parity_p: Some(parity_p_path.clone()),
            parity_q: Some(parity_q_path.clone()),
            rdev_offsets: BTreeMap::new(),
        };

        // Generate Q from the original, uncorrupted data.
        let q_original = crate::array::parity::compute_q(&cfg, 0, SIZE)
            .expect("Q parity calculation must succeed");

        fs::write(&parity_q_path, &q_original).unwrap();

        // Generate correct P parity BEFORE corrupting either data disk.
        let mut p_original = vec![0u8; SIZE];

        for data in &original {
            for (p, d) in p_original.iter_mut().zip(data.iter()) {
                *p ^= *d;
            }
        }

        fs::write(&parity_p_path, &p_original).unwrap();

        // Corrupt the same byte position on two different data disks.
        let mut damaged_1 = original[0].clone();
        damaged_1[1234] ^= 0x5a;
        fs::write(&paths[0], &damaged_1).unwrap();

        let mut damaged_2 = original[1].clone();
        damaged_2[1234] ^= 0xa7;
        fs::write(&paths[1], &damaged_2).unwrap();

        // P reconstruction will produce the corrupted bytes.
        // Q reconstruction should produce the original bytes.
        let golden = original[1].clone();

        let verify: SectorVerifier = Arc::new(move |block: &[u8]| block == golden.as_slice());

        let candidate = Candidate {
            array_phys: 0,
            block_size: SIZE,
            verify,
            raw_phys: 0,
            failing_dev: paths[1].clone(),
            reconfirm: None,
            unreadable: false,
        };

        let stats = run_batch_for_slot(
            &cfg,
            FreezeController::new(None),
            2,
            false,
            vec![candidate],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );

        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.readback_failed, 0);

        // Disk 2 must be repaired to its original contents.
        assert_eq!(fs::read(&paths[1]).unwrap(), original[1]);

        // Disk 1 must still contain its corruption.
        // Recovery of disk 2 must not silently modify disk 1.
        assert_eq!(fs::read(&paths[0]).unwrap(), damaged_1);

        // Disk 3 must remain untouched.
        assert_eq!(fs::read(&paths[2]).unwrap(), original[2]);

        // Both parity disks must remain unchanged.
        assert_eq!(fs::read(&parity_p_path).unwrap(), p_original);
        assert_eq!(fs::read(&parity_q_path).unwrap(), q_original);
    }

    #[test]
    #[ignore = "requires mounted disposable Btrfs fixture and root"]
    fn write_failure_thaws_live_filesystem() {
        use std::io::Write;

        let testdir = fs::read_to_string("/tmp/scrub-rs-parity-testdir")
            .expect("missing parity fixture pointer");
        let mountpoint = PathBuf::from(testdir.trim()).join("mnt");

        assert!(mountpoint.is_dir(), "fixture mountpoint does not exist");

        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();

        let candidates = vec![cand(d1.clone(), 0, 0, false)];

        let mut freeze = FreezeController::new(Some(mountpoint.clone()));
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let _fault = TestFaultGuard::new(TestFault::Write);

        let result = flush_batch(
            &candidates,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("injected write failure must abort the batch"),
        };

        assert!(
            err.to_string().contains("injected write failure"),
            "unexpected error: {err}"
        );

        assert!(
            freeze.take_thaw_error().is_none(),
            "filesystem thaw reported an error"
        );

        assert_eq!(stats.failed, 1);
        assert_eq!(stats.recovered, 0);
        assert_eq!(fs::read(&d1).unwrap(), before);

        // Verify the mounted filesystem is actually writable after the error.
        let probe = mountpoint.join(".recovery-failure-thaw-probe");

        let mut file =
            fs::File::create(&probe).expect("filesystem remained frozen after recovery failure");

        file.write_all(b"recovery failure thaw verified\n")
            .expect("write failed after recovery failure");

        file.sync_all().expect("sync failed after recovery failure");

        drop(file);
        fs::remove_file(&probe).expect("probe cleanup failed");
    }

    #[test]
    fn writer_failures_cancel_without_another_candidate() {
        for fault in [TestFault::Write, TestFault::Readback, TestFault::Thaw] {
            let dir = tempfile::tempdir().unwrap();
            let (cfg, d1) = cfg(&dir);
            let (tx, rx) = mpsc::channel();
            tx.send(BatchMsg::Batch(vec![cand(d1, 0, 0, false)]))
                .unwrap();
            // Keep the sender alive without sending Done or another candidate.
            let cancel = crate::cancel::CancelToken::new();
            let counters = StatusCounters::new();
            let _fault = TestFaultGuard::new(fault);
            let err = run_writer(
                rx,
                cfg,
                FreezeController::new(None),
                Box::new(VerdictStub {
                    stale_tokens: vec![],
                    unverifiable_tokens: vec![],
                }),
                1,
                false,
                Duration::from_secs(60),
                Some(&counters),
                cancel.clone(),
            )
            .expect_err("injected failure must abort");
            assert!(cancel.requested());
            assert_ne!(err.kind(), io::ErrorKind::Interrupted);
            assert_eq!(
                counters.thaw_failed.load(Ordering::Relaxed),
                u64::from(fault == TestFault::Thaw)
            );
            drop(tx);
        }
    }

    #[test]
    fn writer_disconnect_requests_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, _) = cfg(&dir);
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let cancel = crate::cancel::CancelToken::new();
        let err = run_writer(
            rx,
            cfg,
            FreezeController::new(None),
            Box::new(VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            }),
            1,
            false,
            Duration::from_secs(60),
            None,
            cancel.clone(),
        )
        .expect_err("disconnect without Done must fail");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(cancel.requested());
    }

    #[test]
    fn thaw_failure_aborts_writer_before_next_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();

        let (tx, rx) = mpsc::channel();

        tx.send(BatchMsg::Batch(vec![cand(d1.clone(), 0, 0, false)]))
            .unwrap();
        tx.send(BatchMsg::Batch(vec![cand(d1.clone(), 16, 1, false)]))
            .unwrap();
        tx.send(BatchMsg::Done).unwrap();
        drop(tx);

        let reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });

        let _fault = TestFaultGuard::new(TestFault::Thaw);

        let result = run_writer(
            rx,
            cfg,
            FreezeController::new(None),
            reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            crate::cancel::CancelToken::new(),
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("injected thaw failure must abort the writer"),
        };

        assert!(
            err.to_string().contains("injected thaw failure"),
            "unexpected error: {err}"
        );

        let after = fs::read(&d1).unwrap();

        assert_eq!(
            &after[..16],
            &vec![0x44u8; 16],
            "first batch should complete before the simulated thaw error"
        );

        assert_eq!(
            &after[16..],
            &before[16..],
            "second batch must not be processed"
        );
    }

    #[test]
    fn accumulator_discards_partial_batch_on_disconnect() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (batch_tx, batch_rx) = mpsc::sync_channel(1);

        tx.send(Msg::Candidate(cand(
            PathBuf::from("/tmp/test-device"),
            0,
            0,
            false,
        )))
        .unwrap();
        drop(tx);

        run_accumulator(rx, batch_tx, 8, Duration::from_millis(10));

        assert!(
            batch_rx.try_recv().is_err(),
            "disconnected scrub must not forward pending candidates"
        );
    }

    #[test]
    fn accumulator_flushes_partial_batch_on_done() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (batch_tx, batch_rx) = mpsc::sync_channel(1);

        let handle = std::thread::spawn(move || {
            run_accumulator(rx, batch_tx, 8, Duration::from_millis(10));
        });

        tx.send(Msg::Candidate(cand(
            PathBuf::from("/tmp/test-device"),
            0,
            0,
            false,
        )))
        .unwrap();

        tx.send(Msg::Done).unwrap();
        drop(tx);

        match batch_rx.recv().unwrap() {
            BatchMsg::Batch(batch) => assert_eq!(batch.len(), 1),
            BatchMsg::Done => panic!("expected final candidate batch before Done"),
        }

        assert!(matches!(batch_rx.recv(), Ok(BatchMsg::Done)));

        handle.join().unwrap();
    }

    #[test]
    fn cancelled_batch_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();

        let cancel = crate::cancel::CancelToken::new();
        cancel.request();

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();

        let result = flush_batch(
            &[cand(d1.clone(), 0, 0, false)],
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &cancel,
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("cancelled batch must not succeed"),
        };
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&d1).unwrap(), before);
        assert_eq!(stats.recovered, 0);
    }
    #[test]
    fn cancellation_during_recovery_prevents_write() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();

        let cancel = crate::cancel::CancelToken::new();

        let mut freeze = FreezeController::new(None);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(CancellingReconfirmer {
            cancel: cancel.clone(),
        });
        let mut stats = BatchStats::default();

        let result = flush_batch(
            &[cand(d1.clone(), 0, 0, false)],
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &cancel,
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("cancellation during recovery must abort the batch"),
        };

        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(cancel.requested());

        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "cancellation must prevent the recovered block from being written"
        );

        assert_eq!(stats.mismatch, 1, "candidate processing should have begun");
        assert_eq!(stats.recovered, 0, "no repair should be reported");
    }
    #[test]
    #[ignore = "requires root and the mounted disposable Btrfs parity fixture"]
    fn cancellation_thaws_live_filesystem() {
        use std::io::Write;

        let testdir = fs::read_to_string("/tmp/scrub-rs-parity-testdir")
            .expect("missing parity fixture pointer");
        let mountpoint = PathBuf::from(testdir.trim()).join("mnt");

        assert!(mountpoint.is_dir(), "fixture mountpoint does not exist");

        // Never accidentally test against an ordinary directory.
        let output = std::process::Command::new("findmnt")
            .args(["-rn", "-M"])
            .arg(&mountpoint)
            .output()
            .expect("failed to execute findmnt");

        assert!(
            output.status.success() && !output.stdout.is_empty(),
            "fixture mountpoint is not mounted"
        );

        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();

        let cancel = crate::cancel::CancelToken::new();

        let mut freeze = FreezeController::new(Some(mountpoint.clone()));
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(CancellingReconfirmer {
            cancel: cancel.clone(),
        });
        let mut stats = BatchStats::default();

        let result = flush_batch(
            &[cand(d1.clone(), 0, 0, false)],
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_secs(60),
            None,
            &mut stats,
            &cancel,
        );

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("cancellation must abort recovery"),
        };

        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(cancel.requested());

        assert!(
            freeze.take_thaw_error().is_none(),
            "filesystem thaw reported an error"
        );

        assert_eq!(stats.recovered, 0);
        assert_eq!(fs::read(&d1).unwrap(), before);

        // A successful write and sync confirm the filesystem is no longer frozen.
        let probe = mountpoint.join(".recovery-cancellation-thaw-probe");

        let mut file =
            fs::File::create(&probe).expect("filesystem remained frozen after cancellation");

        file.write_all(b"recovery cancellation thaw verified\n")
            .expect("write failed after cancellation");

        file.sync_all().expect("sync failed after cancellation");

        drop(file);
        fs::remove_file(&probe).expect("probe cleanup failed");
    }
}
