# Small personal release: scope decision

This supersedes treating every item in the earlier repair audit as a mandatory
release blocker. Goal: materially improve corruption detection/recovery with
bounded, understood risk, not redesign RotShield or prove infallibility.

## Keep in the first candidate

| Change | Reason |
|---|---|
| Existing METADATA_UUID commit | Required compatibility for the affected filesystem; already independent of the other changes |
| Existing parity reconstruction and checksum/readback verification | The upstream recovery design; preserve it rather than replacing it |
| Cancellation, worker joins, freeze cleanup and fatal-error containment | Makes Stop and error handling work when writes are enabled |
| Runner mount discovery and consistent write authorization | Avoid accidental policy overrides; use the existing Unraid mount/device conventions |
| Local job/problem/progress/final notifications | Directly requested operational visibility; distinguish recoverable from actually repaired |
| Two reconfirmation acceptance guards | A concrete regression reproduced in ordinary temporary-file tests; honor the reader's existing invalid-header flag |
| Matching package/manifest with correct SHA-256 and fork URLs | Necessary to install and retain the actual chosen downstream build |

Keep the lifecycle bundle coherent. Removing pieces in isolation can break its
scanner/runner cleanup protocol. Test source belongs in Git, not the plugin
package. Preserve all existing uncommitted work while organizing it; no reset,
branch, commit, or publishing operation has been performed.

The diff is not a new recovery engine. In batch_recover.rs, the runtime section
is 661 baseline lines versus 833 current lines (including test-only fault hooks),
while the test module grew from 359 to 1,538 lines. Those are section-size/net
comparisons, not a claim that only 172 runtime lines changed. Runner formatting
also obscures its behavioral diff. Separate formatting when organizing commits,
not by blindly overwriting the working file.

## Defer; do not expand this candidate to implement

- A new targeted-repair replay format or file-to-corrupt-sector index.
- General chunk-relocation/mount-identity machinery or an array-state framework.
- A comprehensive fail-closed discovery/parser rewrite.
- A bespoke release-channel system. A correct pinned personal package/manifest
  is required; elaborate publishing automation is not.
- Speculative fixes without a reproduction or a clear relevant execution path.

For the initial intended operating conditions: no concurrent balance,
filesystem resize/device replacement, array rebuild/parity correction, or other
conflicting maintenance during repair. This is an operating restriction to
verify and honor, not a claim the software automatically excludes these tasks.
Do not claim arbitrary concurrent topology changes are supported.

## Narrow reconfirmation experiment: result

`cargo test --test reconfirm_identity` was run **before** the runtime fix:

- Four controls passed: valid metadata confirms the candidate; broken metadata
  checksums refuse it; changed live checksum and freed extent are stale.
- Four identity cases failed: checksum-valid wrong FSID or bytenr in either the
  extent or checksum leaf returned `Corruption` despite reader rejection.

This establishes an incorrect reconfirmation verdict under exceptional metadata
misdirection, not an observed destructive write or ordinary quiet-filesystem
failure. It does not establish the separate relocation hypothesis.

The fix is exactly two guards in `src/btrfs/extent.rs`, each adding
`!r.generation_mismatch` to the existing acceptance condition. No reader rewrite,
new mapping subsystem, or change to parity reconstruction was needed. Afterward:

- **8/8** reconfirmation integration tests pass.
- **66** library tests pass; **3** root-only tests remain ignored.
- The existing **2** checksum integration tests pass.
- Rust formatting and Git whitespace checks pass.

The 51-test shell suite was not repeated for this Rust-only two-guard change;
its previous successful result remains credited, not represented as a new run.

## Next local qualification: operator root step

The existing three ignored tests exercise actual FIFREEZE/FITHAW and verify that
writes can resume after normal cleanup, injected write failure, and cancellation.
They freeze a disposable mounted Btrfs fixture; the recovery fault/cancellation
candidates themselves use temporary regular-file disks. This is useful targeted
cleanup evidence, not an end-to-end repair of files in the mounted filesystem.

`tests/qualify_live_freeze.sh` prepares a new 128 MiB image and available loop
mount, refuses an existing global fixture pointer, invokes only those exact
three tests with timeouts, and thaws/unmounts/detaches on exit. It refuses an
Unraid host. No production device name is supplied. If unmount/detach fails, it
preserves the fixture and prints its location for inspection rather than blindly
removing the backing file. Timeouts cannot guarantee termination of kernel I/O
stuck in an uninterruptible state.

Build the library-test executable as the normal development user first. The
assistant has prepared and listed the actual executable; use the exact path in
the accompanying command, not the normal scrub-rs CLI binary. Run the root step
only in the local WSL checkout. No passwordless sudo setup is needed.

The helper passed Bash syntax and ShellCheck checks. Its three root tests have
**not** been run in this session. If the fixture check passes, proceed to curated
personal packaging and a supervised rollout; do not open another broad hardening
programme first. Actual server mapping/options, package compatibility, local
notifications, and the transition to repair must still be verified/approved.

## Qualification and local package follow-up

The operator subsequently ran the prepared local fixture check successfully:
all three exact root-only tests passed (normal freeze/thaw, write-failure thaw,
and cancellation thaw). The injected write-failure message was expected.
The fixture pointer was absent afterward and the fixture loop had no mount
entry when inspected. This is operator-run evidence, not a claim the assistant
executed sudo or an end-to-end mounted-file parity repair.

A local personal candidate was then prepared as version 2026.11.19a in
plugins/archive/personal-2026.11.19a (ignored build output). It uses the existing
packager on an explicit temporary runtime staging tree. No build/release
framework was added and the checked-in upstream manifest was not rewritten.

The release-profile run passed 66 library tests and 10 integration tests;
3 root-only tests were ignored in that run and are covered separately by the
operator's debug-profile fixture result. All 51 shell tests passed again.
The extracted package passed file-hash checks, Bash syntax, ShellCheck, and
scanner --help. Its highest required glibc symbol is GLIBC_2.34.

The candidate includes 13 runtime files and a source/binary build record; no
craft-corrupt, harnesses, or scratch files. The generated manifest pins the
actual archive SHA-256 and fork asset URLs. Its update URL is pinned to this
same release, requiring intentional manual upgrades instead of following
upstream or a moving latest release. This supersedes the earlier optional
channel-branch recommendation for the first personal build.

The working source remains uncommitted; build-info.json records its input-file
hashes and explicitly labels it dirty rather than pretending HEAD contains the
whole build. Before publishing, organize/commit the reviewed source and choose
the intended release source. No branch/tag/release/install was created, and
candidate URLs are not published or expected to download successfully yet.

Next: verify novinha's glibc, idle state, and existing configuration read-only;
then prepare the personal source/release and an idle-window install/rollback.
Do not enable repair as a side effect of installation.

## Source organization and personal publication

The approved personal/2026.11.19a branch now separates Rust lifecycle changes,
the small reconfirmation fix, runner/notification behavior, Clippy-compatible
syntax cleanup, and fork-only packaging/publication. The existing METADATA_UUID
commit remains independently identifiable. Strict Clippy now passes without
allowances; the two baseline warnings required only equivalent syntax changes.

The user's API work remains local and unchanged. In the overlapping runner file,
the qualified pre-API bytes were staged directly into Git's index; the working
file was not replaced. The status.php refactor and new API results/parser files
are not in the release source or package. All preexisting work was snapshotted
locally before staging. No stash, reset, force push, or production change was used.

The existing GitHub workflow is corrected to publish a generated manifest with
the finished package's actual SHA-256, matching source/binary build provenance,
and checksum list. Its allowlist rejects development artifacts. A local test of
the exact workflow packaging steps on a clean Git export passed, including
intentional extra API/harness files that were excluded from the package.
The source manifest is a template; install only the generated release asset.

Production glibc 2.44, idle state, available utilities, monthly expression,
WRITE=0, and empty extra options were checked by the operator. Publication and
installation are separate: installing must preserve WRITE=0, and the first
repair-enabled production run remains a separate supervised decision.
