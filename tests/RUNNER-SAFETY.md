# Runner policy and fatal-repair fail-stop qualification

This packet closes the runner-authorization and job-level fatal-failure gaps
identified in `REPAIR-QUALIFICATION.md`, subject to the remaining live/root
qualification below. It does **not** approve production repair.

## Policy and compatibility

- `WRITE` is either `0` or `1` (unset defaults to `0`; explicitly empty/other
  values refuse the run). Only `WRITE=1` supplies `--repair`.
- Plugin extra options are limited to `--status-sock`, `--batch-max`,
  `--batch-idle`, and `--freeze-max`. Both `--flag value` and `--flag=value` are
  accepted and normalized to the CLI's separate flag/value form.
- Reject duplicate flags, missing/invalid values, unknown flags, operands,
  newlines, and plugin-owned repair/freeze/offset overrides before starting a
  scanner or sending a started notification. In particular, old configurations
  with `--repair`, `--no-freeze`, `--freeze-mount`, or `--offset` in extra options
  must be corrected; they are not silently ignored.
- Extra options are plain whitespace-delimited tokens, not a shell command.
  Shell quoting/escaping is not interpreted. Arguments are passed using arrays;
  metacharacters do not execute or expand. Auto-detected mount paths retain
  whitespace and metacharacters as one argument.
- `--batch-max` accepts positive decimal integers of at most nine digits.
  `--batch-idle` accepts nonnegative plain decimal seconds; `--freeze-max`
  requires positive plain decimal seconds, each at most twelve characters.
  Scientific notation, NaN/infinity, signs, and empty values are rejected.
- Repair requires a resolved live mountpoint. Standalone offline CLI behavior
  is unchanged. Mount identity and live physical mapping still need the separate
  qualification tracked in `REPAIR-QUALIFICATION.md`.

## Fatal repair outcome policy

Repair jobs continue through ordinary data-result codes `0`, `3`, `4`, `5`.
Runtime/usage errors, metadata fatal (`6`), unexpected exits, missing completion,
or log-collector failure stop the job before another scanner launches. Missing
completion with zero exit becomes a nonzero failure. Read-only runtime errors
retain the previous continue-and-report behavior.

A partial-results alert distinguishes an aborted repair job from clean
completion. Fatal writer errors now immediately request shared cancellation,
without waiting for another candidate to discover a disconnected writer.
The underlying writer error is reported before generic cancellation.

### Machine-readable cleanup evidence

- `thaw_failed=1`: sticky, an actual thaw attempt failed and requires manual
  verification, even if a later retry succeeds. This does not assert the
  filesystem is definitely still frozen. It is included in status snapshots
  and emitted before post-join fatal result handling.
- `repair_cleanup=complete`: emitted after workers join only when the writer
  did not panic and no sticky thaw failure exists. Ordinary writer errors can
  still finish guard cleanup; early setup exits emit no such marker.
- `scrub complete:`: normal scanner completion, not by itself proof of a clean
  disk. Data-result exit codes still determine the outcome.

Unexplained incomplete repair output retains the safety lock unless cleanup
was explicitly confirmed. An explicit thaw failure or collector failure retains
it regardless of an otherwise successful exit. A runtime failure with confirmed
cleanup aborts the job but does not unnecessarily retain the lock.

The interlock uses the existing `runs/.lock/stop-pending` marker and adds a
`safety-reason` explanation. `running` remains 1, and new runs refuse to reclaim
that lock, even after the runner has exited. There is intentionally no automatic
clear/retry: inspect scanner processes and filesystem state first.

## Stop/launch synchronization

A persistent `runs/.launch.lock` serializes scanner publication, Stop's pending
marker/descendant snapshot, and final job-record publication. Scanner PID,
active device, discovered freeze path, and per-run write mode/socket are
recorded so Stop does not rely on config edited during the run or a previous
disk inferred from an older snapshot.

A private `/tmp/rotshield-scan.*` FIFO streams output through a collector. Stop
sends TERM to the scanner tree first, leaves the runner/collector alive, and
waits for output to drain before inspecting cleanup records. This preserves
late thaw errors printed while Rust joins its workers. The previous immediate
TERM of the entire tree could discard that evidence.

After a ten-second grace, still-running tracked processes are killed. Emergency
thaw is considered only for a repair scanner that actually launched and lacks
clean current-scanner cleanup evidence. Killing a blocked discovery/collector
process does not by itself justify thawing a filesystem. Read-only runs never
perform emergency thaw, even if the saved config was changed to `WRITE=1`.

Emergency thaw uses the recorded active freeze path and is bounded to three
seconds plus one-second kill grace. Failure/timeout/unknown mount retains the
interlock and reports `SAFETY_ABORT`, not successful cancellation. Any explicit
thaw failure is authoritative. Cleanup records are scoped to the last scanner
section; one disk's successful cleanup cannot certify another disk.

This is **not** the independent FSID/device/mount authentication still required
before unattended writes. A recorded/discovered path is stronger association,
not proof of filesystem identity.

## Validation

Executed against this packet:

- `python3 tests/notifications_test.py`: **51 tests passed**, approximately
  135 seconds, including all prior notification tests, policy injection/refusal,
  literal argv, fatal repair halt, retained interlocks, delayed TERM output,
  missing/clean cleanup markers, previous-disk marker isolation, changed config,
  Stop during discovery, forced read-only stop, successful/failed/hung emergency
  thaw, and escalation before any scanner or between clean disks.
- `cargo test --all-targets`: **66 library tests passed, 3 root-only tests
  ignored; 2 integration tests passed**. Binary test targets contain no tests.
- `cargo fmt --all -- --check`: passed.
- Bash syntax and ShellCheck warning checks for runner, both notify helpers,
  and rc script: passed. `git diff --check`: passed.
- Built the actual debug scanner and ran a non-root read-only smoke test on a
  newly created 128 MiB regular-file Btrfs image containing 4 MiB of data.
  Exit 0, both completion/cleanup markers observed, image SHA-256 unchanged.
  The temporary image was never mounted or attached to a loop device.
- Strict Clippy is blocked by preexisting `needless_bool` in
  `btrfs/reader.rs` and `collapsible_if` in the existing main cancellation code.
  New test lints were fixed. Clippy passed with only those two baseline lints
  explicitly allowed; this is not a claim of an unqualified strict-Clippy pass.

Mock scanners and fsfreeze mocks never access real devices or freeze a real
filesystem. Their success does not replace the following root qualification.

## Remaining before deployment / repair enablement

- Repeat targeted **disposable mounted-Btrfs real-scanner** cancellation/thaw
  qualification because launch/Stop behavior changed. The current session has
  no passwordless sudo; no ignored/root-only tests were counted as passing.
  Existing root tests use `/tmp/scrub-rs-parity-testdir` as a fixture pointer;
  independently verify that pointer and mount refer to disposable fixtures
  before running them. Never point them at production mounts.
- Deploy matching runner/helper/scanner versions together when eventually
  allowing repair. Older scanners without explicit cleanup records fail closed
  on ambiguous shutdown; do not promise their repair Stop will release the lock.
  Read-only notification rollout can remain a separately qualified step.
- Reconfirm metadata trust, freeze filesystem identity, current physical mapping,
  strict repair array preconditions, and coverage/final status consistency remain
  open. Packaging/update SHA-256 and fork-channel work also remain open.
- No production config, disk, scheduler, plugin install, Git commit/tag/branch,
  or GitHub release was changed. Automatic production repair remains disabled.
