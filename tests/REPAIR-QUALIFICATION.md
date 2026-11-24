# Unattended repair qualification

Assessment date: November 18, 2026. Source reviewed in the current dirty checkout
based on `3bc673c`. **Not approved for unattended production repair.** Keep
`WRITE=0` and inspect `EXTRA_OPTIONS`; today that setting alone is not an
independent read-only interlock.

This is a code/evidence inventory, not a claim that every risk has been reproduced.
No destructive/root tests or production operations were performed for this review.
The next implementation work should be narrowly scoped to the confirmed gaps.

## Scope superseded by the release review

[RELEASE-CANDIDATE.md](RELEASE-CANDIDATE.md) is the current, narrower release plan.
The audit below is an inventory, not a requirement to implement every hardening
idea. It also records the reproduced two-guard reconfirmation fix and which
broader concerns are deferred under explicit operating restrictions.

## Evidence levels

- **Locally executed:** the preceding notification qualification ran 18 mock
  lifecycle tests and 63 Rust library tests successfully; 3 root-only library
  tests were ignored. See `NOTIFICATIONS.md`. A library pass includes tests using
  regular temporary files, not just pure algorithms.
- **Prior handoff evidence:** real-scanner Stop on a disposable loopback Btrfs
  fixture passed, including unchanged image hash and lock removal; root-only
  `cancellation_thaws_live_filesystem` passed; earlier P/Q and live-repair fixture
  tests were reported successful. Do not discard these results or rerun the whole
  shutdown suite without a relevant change. Exact build/config artifacts should
  be associated with a release candidate before claiming release qualification.
- **Source review:** gaps below were identified by reading code. They are not
  new production incidents. A missing assertion/test is distinguished from a
  demonstrated unsafe code path.
- **Unverified:** real Dynamix delivery, downstream package/install persistence,
  release-candidate compatibility on Unraid, current production mapping/config,
  and unattended repair.

## Runner safety implementation update

The first two work packets now have local implementation and regression evidence:
plugin-owned extra-option/write policy, immediate writer cancellation, fatal
repair-job halt, and explicit cleanup/thaw interlocks. Stop and scanner launch
are synchronized; delayed shutdown output is drained before evaluating safety.
See [RUNNER-SAFETY.md](RUNNER-SAFETY.md) for the 51-test mock result, Rust results,
real read-only image smoke test, compatibility changes, and root-only follow-up.

The detailed source findings below describe the initial review, not unfixed
claims against the updated authorization/Stop implementation. Filesystem identity,
metadata trust, live physical mapping, and strict array preconditions remain open.
Unattended production repair is still not approved.

## Gating matrix

| Gate | Existing evidence | Remaining qualification / minimal work | Status |
|---|---|---|---|
| Explicit write authorization | `build_args()` adds `--repair` for `WRITE=1`; notification tests check normal read-only argv | Reject policy-overriding extra flags and duplicate/alternate forms. Construct/validate effective arguments so `WRITE=0` cannot be bypassed. | **Implemented; local regressions pass** |
| Live mount belongs to target | Runner resolves raw member -> md/nmd partition -> mount; missing mount refused for structured repair | Bind Btrfs filesystem identity, disk slot/raw device, offset, and mount together; wrong mount must refuse writes. Use the same opened mount for freeze and thaw. | **Confirmed identity-validation gap** |
| Reconfirmation metadata is trustworthy | Reader checks checksum/header identity; normal tree walker rejects header-invalid fallback nodes | Reconfirmation descents must reject header-invalid nodes and enforce parent generation/level/owner expectations. | **Confirmed blocker** |
| Physical mapping is current | Logical ownership/checksum reconfirmation, source reread, per-disk offsets | Reconfirm current chunk/device/physical mapping under freeze, not just logical checksum using startup chunk map. Changed/unknown mappings must never authorize writes. | **Confirmed validation gap; relocation regression needed** |
| Fatal repair errors stop job | Rust stops its writer on write/cache/readback/thaw failure; batch fault-injection tests | Request cancellation immediately on writer failure; prevent next-disk repair after fatal failure; retain an interlock when thaw is unresolved. | **Implemented; live/root requalification open** |
| Forced shutdown/thaw uncertainty | Graceful cancellation and retained lock for still-alive processes; prior Stop tests | Failed emergency thaw currently can still become ordinary cancellation with lock removal. Distinguish known thawed/offline state from uncertainty and keep the safety interlock. | **Implemented; live/root requalification open** |
| Array identity/layout is valid | Per-member offsets and P/Q/PQ fixtures; filesystem-magic canary | Add strict repair-mode config validation, checked address arithmetic/bounds, identity stability, and array-operation exclusion. Test missing/invalid layout inputs. | **Confirmed validation gaps; server state policy unqualified** |
| Verification and reporting | Writer verifies reconstructed data, fsyncs, invalidates raw cache, verifies readback; notifications separate assessments/writes | Ensure final errors/counters include reconfirmation failures; incomplete coverage must not look clean. Add machine-readable repair/fatal safety outcomes where needed for interlocks. | **Partially qualified** |
| Scheduling and notifications | Monthly scheduling already in use; 18 mock notification tests passed | Deploy/observe actual local delivery, preserved config/schedule, update and reboot persistence. | **Local pass; operational qualification open** |
| Exact release candidate | Source and tests available | Clean recorded source, matching packaged/tested binary, SHA-256-correct downstream manifest, retained rollback package. | **Release gate open** |

## Confirmed code findings

Line numbers are review anchors and may shift as fixes land.

### 1. Extra options bypass structured policy

`plugins/source/rotshield/usr/local/emhttp/plugins/rotshield/scripts/scrub.sh`:

- `build_args()` around lines 78–86 appends unrestricted `EXTRA_OPTIONS`.
- Repair refusal around lines 371–388 tests `WRITE=1`, not effective repair argv.
- `--no-freeze` bypasses the entire auto-freeze/refusal block.

A temporary mock-lifecycle reproduction during this assessment confirmed that
WRITE=0 with both --repair and --no-freeze in extra options reaches the mock
scanner with those flags and without a freeze mount. The fixture completed and
cleaned up normally; no real scanner, block device, or freeze was used.

Thus `WRITE=0` plus `EXTRA_OPTIONS='--repair'` can request writes without the
structured repair mount-refusal gate. The CLI also accepts explicit freeze
paths and offset options. This is an actual authorization mismatch, not merely
missing test coverage. Do not rely on a notification's mode label as enforcement.

Minimal next change: plugin-managed runs should own write mode, freeze policy,
freeze target, target device, and partition offset. Limit extra options to known
tuning/status options; reject unknown/policy-changing options before starting a
job. Preserve the standalone offline CLI as a separate, explicitly authorized
workflow. Do not silently reinterpret old unsafe options.

Required regressions: `WRITE=0 + --repair`, `--no-freeze`, supplied freeze mount,
offset overrides, duplicate flags, supported tuning/status options, refusal
before scanner launch, and unchanged `WRITE=0` behavior. Avoid a substring-only
blacklist that misses `--option=value` or value/argument boundary cases.

### 2. Freeze does not authenticate mount identity

`freeze.rs:28–59` trusts a supplied pathname. `freeze_path()` and `thaw_path()`
(`freeze.rs:159–177`) each reopen that pathname; no filesystem/device identity
check or retained mount FD binds these operations together. `main.rs:368–384`
treats an undeclared mount as an offline/no-op freeze controller.

A successful FIFREEZE of an unrelated filesystem is not protection for the target.
Minimum gate: independently validate target FSID/metadata UUID relationships and
mount/raw/array mapping, and freeze/thaw through the same retained mount handle.
Refuse uncertainty; never freeze arbitrary unrelated mounts as a probe. Tests
must cover the raw-rdev versus mounted-md-partition distinction and offset.

A verified offline path may remain available for disposable fixtures/standalone
operation; an absent mount declaration is not proof that a live raw device is
offline. Root fixture coverage is required when these semantics change.

### 3. Metadata fallback is acceptable for diagnosis, not write permission

`btrfs/reader.rs:230–251,278–282` can return a checksum-valid fallback node whose
header validation failed, marked `generation_mismatch=true`. Validation includes
FSID and bytenr (`reader.rs:296–320`), so that flag is broader than its name suggests.

`btrfs/extent.rs:132–143` and its checksum descent around lines 219–230 reject
only `all_mirrors_failed`, ignoring `generation_mismatch`; they pass
`GEN_DONT_CHECK` and no expected owner/level. `reconfirm_mismatch()` can therefore
consume such metadata when deciding whether a queued write is still justified.

Fix the write-authorization path to return `Unverifiable` on identity/header
failure, and carry correct tree generation/level/owner expectations. Do not
change diagnostic traversal indiscriminately. Add checksum-valid wrong-FSID,
wrong-bytenr, and stale parent/child fixtures. Stubbed `Unverifiable` verdict tests
alone do not establish the actual Btrfs reconfirmer rejects these inputs.

### 4. Startup physical map is reused for live reconfirmation

`btrfs/scrub_driver.rs:325–327` clones the startup chunk map. The reconfirmer
refreshes superblock/tree roots but uses that same map (`:339–382`). Requests
carry the logical token and checksum; the writer uses the originally queued
physical location (`batch_recover.rs:600–605`).

This does not establish that relocation left the queued physical sector mapped
to that logical extent. A logical checksum match is not a physical-ownership
check. Refresh/validate mapping under the authenticated freeze and compare the
candidate's device/range/physical-copy identity. A conservative refusal on
relevant layout changes is preferable to attempting a speculative write.

Required test: change the logical-to-physical mapping between detection and
write-time reconfirmation while preserving logical contents/checksum. The old
physical range must not be written. Do not test this by balancing production.

### 5. Fatal repair error containment is per writer, not yet per job

`batch_recover.rs:280–294,305–319` returns writer failures without immediately
requesting the shared cancellation token. The scanner requests cancellation if a
later candidate send sees the disconnected pipeline (`main.rs:349–351`); a later
candidate may not arrive promptly.

The runner's loop (`scrub.sh:464–476`) proceeds to the next disk unless Stop is
pending. Its `errored` flag changes the final label but not this continuation.
`tests/notifications_test.py::test_aborted_disk_sends_one_problematic_job_summary`
currently verifies the second disk completes after the first aborts. That is
existing behavior, not evidence for a repair fail-stop policy.

Define a conservative repair-job policy: fatal runtime/write/readback/thaw
failure halts the entire job immediately; ordinary checksum findings need not
be confused with infrastructure failure. Make safety state machine-readable
rather than inferring an unresolved thaw from generic exit code 1. Retain a
lock/interlock for unresolved thaw, not for every harmless setup error.

Emergency shutdown also needs attention: `thaw_frozen_mounts()` around
`scrub.sh:654–658` echoes an ambiguous fsfreeze failure and can return success;
Stop around `:775–830` can then record cancellation and remove the lock even
when thaw is uncertain. Failure is not equivalent to “already thawed.” Preserve
known-offline/no-freeze cases without inventing success for ambiguous cases.

Required regressions: no next scanner after a fatal repair error, immediate
scanner cancellation on writer error, known-safe versus unresolved-thaw lock
handling, no false clean/cancelled outcome, and notification of the safety fault.
Changing Stop/freeze behavior justifies targeted reruns of the prior disposable
real-scanner cancellation/thaw tests.

### 6. Strict repair-mode array validation

`array/config.rs:123–135` silently omits named non-block members and defaults
missing/malformed offsets to zero. Offset conversion is unchecked multiplication.
`array/stripe.rs:86,119–127` adds offsets without checked arithmetic; writes
reopen the target pathname and have no explicit target-range/capacity guard.

These observations do not prove the normal novinha layout is wrong. They show
that permissive discovery is currently used where fail-closed write authorization
is needed. Keep read-only discovery usable, but require complete supported layout,
valid offsets, target identity and bounded arithmetic for writes. Account for
legitimately empty slots rather than demanding that every possible slot exists.

Define and test exclusions for active parity rebuild/sync, device replacement,
array topology changes, and unsupported filesystem/data profiles before enabling
scheduled writes. Exact Unraid state fields and behavior need confirmation on the
supported version; do not invent proc keys. A filesystem-magic canary is useful
but does not prove layout stability or mount identity.

## Existing tests to retain and extend

All below are source-present; non-ignored library tests were included in the
recorded 63-pass run. Tests using temporary regular files are not equivalent to
mounted Btrfs, real block-device cache invalidation, or Unraid device topology.

| Area | Test names | Remaining distinction |
|---|---|---|
| Freeze refusal | `freeze_failure_defers_writes_into_not_frozen` | Synthetic failure; not wrong-filesystem identity |
| Stale/unverifiable candidates | `verdicts_stale_and_unverifiable_are_counted_and_never_written` | Stub verdicts; real reconfirmation validation still needed |
| Write/readback/cache halt | `write_failure_aborts_batch_without_writing_later_candidates`, `readback_mismatch_aborts_batch_without_writing_later_candidates`, `readback_io_failure_aborts_batch_without_writing_later_candidates`, `cache_failure_aborts_batch_without_writing_later_candidates` | Per-batch fixtures, not whole-job halt |
| Deferred work | `freeze_timeout_defers_the_rest_of_the_batch` | Checks between candidates; not a hard timeout for blocked kernel I/O |
| Recovery writes | `integration_4k_p_recovery_repairs_corrupted_sector`, `integration_4k_q_recovery_when_p_baked_in`, `integration_4k_two_disk_corruption_recovers_via_pq` | Regular-file parity fixtures |
| No-write outcomes | `integration_4k_unrecoverable_corruption_never_writes`, `integration_4k_dry_run_recovers_without_writing` | Retain byte-level before/after assertions |
| Thaw halt | `thaw_failure_aborts_writer_before_next_batch` | Injected fault; not actual failed FITHAW or retained shell interlock |
| Cancellation/channel lifecycle | `cancelled_batch_does_not_write`, `cancellation_during_recovery_prevents_write`, `accumulator_discards_partial_batch_on_disconnect`, `accumulator_flushes_partial_batch_on_done` | Extend immediate writer-to-scanner cancellation |
| Frozen state | `freeze::tests::failed_thaw_is_recorded_and_keeps_frozen_state` | Synthetic nonexistent path |
| Offsets/cache | `gather_adds_per_disk_rdev_offset`, `cache_hit_only_at_exact_generation` | Not complete layout parsing or relocation validation |
| Config | `parses_slots_and_offsets` | Currently splits sample text/asserts strings, not production parser validation |

Root-only ignored tests: `live_freeze_thaw_smoke_test`,
`write_failure_thaws_live_filesystem`, `cancellation_thaws_live_filesystem`.
Only the last has an explicit prior pass recorded in the handoff. Do not count
ignored tests as passing; preserve any additional existing external test records.

## Reporting and scope

Notifications already separate assessment from verified repairs using explicit
Rust log records. Raw readback success does not invalidate mounted-file page
cache; retain the reboot/remount advisory. `state=done` means finished, not clean.

Before unattended repair, ensure shared reconfirmation metadata counters are
reflected consistently in final totals/exit severity (`btrfs/open.rs` updates
shared counters; `main.rs` bases final decisions on scan statistics). Missing
chunk mappings are currently logged/skipped in `btrfs/scrub.rs:262–272`; incomplete
coverage needs an explicit outcome, not an implied clean bill of health.

Current parity writer operates on verifiable data candidates. Do not describe it
as general Btrfs metadata repair. Unsupported/unverifiable corruption must alert
and remain unwritten; absence of a repair is sometimes the correct safety result.

## Ordered work packets

1. **Enforce runner policy:** effective argv validation, `WRITE=0` interlock,
   plugin-owned freeze/offset/device settings, focused mock regressions.
2. **Contain fatal failures:** immediate shared cancellation, repair-job fail-stop,
   explicit unresolved-thaw interlock/outcome; extend shell and Rust tests.
3. **Authenticate write authorization:** reject unsafe reconfirmation metadata,
   validate/pin freeze filesystem identity, and revalidate physical mapping.
   Add real parser/tree fixtures and disposable live-filesystem tests.
4. **Validate array preconditions and coverage:** strict write-mode layout/bounds,
   conflict exclusion, consistent final safety/coverage outcomes.
5. **Produce a pinned downstream read-only candidate:** use
   `../plugins/DOWNSTREAM-RELEASES.md`; actual delivery/install/persistence tests
   can proceed while repair qualification remains open.
6. **Qualify the exact repair candidate on disposable fixtures:** rerun only tests
   affected by changes plus the end-to-end P/Q/live-repair acceptance set. Record
   source, binary hash, options, fixture topology, byte-level outcomes and thaw.
7. **Production rollout:** verified read-only install first, then explicit approval
   for a supervised repair-enabled run, then unattended monthly enablement only
   after reviewing those results. Never induce corruption on production.

No automatic production repair enablement, mass refactor, repository publishing,
or blanket rerun of destructive integration suites is authorized by this document.
