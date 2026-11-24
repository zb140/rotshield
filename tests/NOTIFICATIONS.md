# Notification qualification and deployment

## Local validation — November 18, 2026

From the repository root:

```sh
python3 tests/notifications_test.py
cargo test --manifest-path scrub-rs/Cargo.toml --lib
```

Observed: **18 notification tests passed; 63 Rust library tests passed, 3 root-only tests ignored**. Bash syntax checks and ShellCheck at warning severity passed for `scrub.sh`, `notify-common.sh`, `notify-monitor.sh`, and `rc.rotshield`. Git diff whitespace checks passed.

The Python-standard-library harness uses temporary directories, gated mock scanners serving real Unix HTTP sockets, a controllable clock, and an argument-recording notifier. It never accesses block devices or freezes filesystems. Mock repair cases qualify notification accounting, not real repair.

Coverage includes manual/scheduled start details, duplicate/empty-target suppression, sequential completion, per-disk/category alert deduplication and escalation, six-hour/date/9am daily boundaries, completion counting, cancellation partial results and lock release, aborted-scanner severity, failing/hung notifiers, terminal-window Stop behavior, silence after completion, cron/config preservation, and stale/truncated snapshot rejection.

The previously passed real-scanner loopback shutdown/thaw suite was not repeated. No Rust code was changed for this notification work.

## Semantics and limits

- Delivery uses the local Dynamix notify executable; no new production service, Python dependency, or cloud dependency.
- The monitor polls once per minute. Immediate alerts mean the next successful poll, normally within about a minute. A scan finishing between polls still reports its issues in the final summary.
- Each disk/category alerts once at its first occurrence and once at escalation to 100 affected items. Recovery, freeze, and readback failures alert on first observation.
- Daily summaries run at the first poll at/after 09:00 local time on a later calendar date, after six elapsed hours, once per qualifying day.
- Delivery is at-most-once **attempted**, not guaranteed. Failed attempts are not retried every minute. Individual delivery is bounded to three seconds plus one-second kill grace; the monitor has a ten-second timeout plus kill grace. Missing notify/timeout skips delivery without stopping scans.
- Rust `recovered` includes dry-run reconstructions and verified writes. `not_frozen` is recoverable-but-unwritten. `readback_failed` is not a verified repair. `repaired_while_mounted` is a boolean write/cache advisory, not a count.
- Verified repairs and dry-run recoverable counts use Rust's unconditional per-candidate log records. Missing/truncated logs may undercount; notifications are not a repair audit. Mounted-file page-cache warnings remain visible.
- Summaries require the final `progress_pct` snapshot field, replace rather than merge per-device snapshots, and protect completed disks from stale live responses. Execution remains sequential.
- The monitor is sealed/drained before the terminal log marker. Cancellation notification follows the existing tracked-process shutdown check; unresolved shutdown retains the lock and does not claim completion.

## Production deployment checklist — not executed

1. Use a maintenance window with no active job. On `novinha`, inspect:

   ```sh
   /usr/local/emhttp/plugins/rotshield/scripts/scrub.sh running
   ls -la /boot/config/plugins/rotshield/runs/.lock
   grep -E '^(SCHEDULE|CRON|RECOVER|WRITE|EXTRA_OPTIONS)=' /boot/config/plugins/rotshield/config.cfg
   cat /etc/cron.d/rotshield
   command -v bash curl flock timeout sha256sum awk
   test -x /usr/local/emhttp/webGui/scripts/notify
   ```

   `running` must be 0; an absent lock directory is normal when idle. Never clear an unresolved `stop-pending` lock without checking scanner processes and filesystem state.

2. Preserve the confirmed intended configuration:

   ```text
   SCHEDULE='custom'
   CRON='0 3 5 * *'
   RECOVER=0
   WRITE=0
   ```

   Also inspect `EXTRA_OPTIONS`: it must not independently enable `--repair`. The runner currently assesses recovery by default even with `RECOVER=0`; assessment does not authorize writes. **Do not enable production repair.**

3. Save a local rollback copy of installed scripts, `/etc/rc.d/rc.rotshield`, `config.cfg`, and cron files, plus the previous working plugin package for reboot-persistent rollback. This is not a request for a full-array backup.

4. Stage these four matching files together, syntax-check them, and install only while idle:
   - `scripts/scrub.sh`
   - `scripts/notify-common.sh`
   - `scripts/notify-monitor.sh`
   - `/etc/rc.d/rc.rotshield`

   Make `scrub.sh`, `notify-monitor.sh`, and `rc.rotshield` executable (0755); `notify-common.sh` needs read permission (0644 is sufficient). Do not replace the scanner binary during this shell-only deployment. Do not overwrite the qualified checkout with the draft/revised archive.

5. For reboot persistence, use the normal versioned plugin package/update path. `pkg_build.sh` includes new shell files and normalizes executable permissions. Curate a release staging tree: its broad file inclusion would otherwise package the local stop harness. Exclude test harnesses, `craft-corrupt`, local output, and development artifacts. Follow the existing `.plg` version/hash process. Package build/install and reboot persistence are **not yet tested**.

6. Reapply scheduling after installation:

   ```sh
   /etc/rc.d/rc.rotshield restart
   cat /etc/cron.d/rotshield
   cat /etc/cron.d/rotshield-monitor
   grep -E '^(SCHEDULE|CRON|RECOVER|WRITE)=' /boot/config/plugins/rotshield/config.cfg
   ```

   Scan cron must retain `0 3 5 * *` and add `ROTSHIELD_SCHEDULED=1` before the runner command. Monitor cron runs once per minute, including for manual scans when automatic scheduling is disabled. Config must remain unchanged.

7. Run an idle monitor smoke test; it must not produce a scan notification. Optionally issue one explicitly labeled local Dynamix delivery test:

   ```sh
   /usr/local/emhttp/plugins/rotshield/scripts/notify-monitor.sh
   /usr/local/emhttp/webGui/scripts/notify -e rotshield_deploy_test -s 'RotShield notification test' -d 'Delivery test only; no scan or repair started' -i normal
   ```

   Verify local notification display. Start a production read-only scan only when explicitly desired, then observe start/progress/completion. Never simulate corruption or run repair tests on production. Actual Dynamix delivery and a complete production scan remain unverified.

8. Roll back only while idle: restore matching prior scripts/rc/package and cron state, remove monitor cron if absent previously, reapply the old schedule, and recheck `WRITE=0`. Preserve logs and never delete a live/unresolved run lock to force rollback.

## Proposed contribution boundaries — no commits created

The current base is `3bc673c` (METADATA_UUID support); local and remote `fix/btrfs-metadata-uuid` refs point there. Verify upstream PR status separately: refs do not establish whether a PR is open.

1. **METADATA_UUID:** existing independent commit/review.
2. **Rust recovery safety/cancellation:** existing changes in `cancel.rs`, `batch_recover.rs`, `freeze.rs`, `fs.rs`, Btrfs scrub/driver, `main.rs`, `lib.rs`, `Cargo.toml`, and `array/config.rs`. Split independent array/recovery enhancements only where tests establish independence. Keep safety tests with owning changes.
3. **Runner shutdown/mount safety:** existing `scrub.sh` mapping, repair refusal, tracked-process Stop, retained locks, and thaw changes. Stage by hunk because notification edits share this file.
4. **Job notifications:** both notify helpers, notification-only runner changes, rc origin/monitor cron, `notifications_test.py`, and this note. Shared-helper sourcing and lifecycle calls must land together. Formatting-only reindentation already present in the runner obscures review; separate it only after preserving/reviewing the behavioral diff.
5. **Release integration:** curated package, version/hash changes, and deployment evidence after qualification. The non-root mock suite is manually runnable; optionally wire it into existing CI.

Do not blindly commit `out.txt`, Zone.Identifier streams, downloaded archives, target binaries, or ad-hoc stop/root harnesses. Review which fixtures should become durable tests. No production repair enablement belongs in these commits.

## Follow-up repair safety review

The notification pass does not qualify unattended repair. See
[REPAIR-QUALIFICATION.md](REPAIR-QUALIFICATION.md) for the subsequently identified
authorization, metadata/mapping, freeze identity, and fail-stop gaps, and
[the downstream release plan](../plugins/DOWNSTREAM-RELEASES.md) for fork
packaging/update requirements. WRITE=0 must also be accompanied by safe extra
options until the runner authorization gap is closed.

## Runner safety packet follow-up

See [RUNNER-SAFETY.md](RUNNER-SAFETY.md) for subsequent authorization/fail-stop
changes, current 51-test coverage, matching scanner cleanup-marker requirements,
and mandatory disposable mounted-Btrfs requalification. The earlier 18-test
result above remains historical notification evidence, not the current suite size.
