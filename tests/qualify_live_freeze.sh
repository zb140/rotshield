#!/bin/bash
# Local test only: creates its own disposable image/mount. Never run on Unraid.
# Supply an already-built Rust LIBRARY test executable, not the scrub-rs CLI.
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: sudo bash $0 /absolute/path/to/library-test-executable" >&2
    exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
    echo 'Root is required only for this disposable local mount/freeze test.' >&2
    exit 2
fi
if [ -d /usr/local/emhttp ]; then
    echo 'Refusing to run on an Unraid host; use the local WSL test environment.' >&2
    exit 2
fi
for cmd in realpath mktemp truncate mkfs.btrfs losetup mount mountpoint fsfreeze umount timeout; do
    command -v "$cmd" >/dev/null || {
        echo "Missing utility: $cmd" >&2
        exit 2
    }
done
binary="$(realpath -e "$1")"
[ -f "$binary" ] && [ -x "$binary" ] || {
    echo 'Test executable missing/not executable' >&2
    exit 2
}
# Existing tests use this pointer. Never replace a previous fixture or symlink.
pointer=/tmp/scrub-rs-parity-testdir
if [ -e "$pointer" ] || [ -L "$pointer" ]; then
    echo "Existing fixture pointer found: $pointer; refusing to replace it." >&2
    exit 2
fi
# Verify the supplied executable actually contains the expected library tests.
tests=(
    freeze::tests::live_freeze_thaw_smoke_test
    batch_recover::tests::write_failure_thaws_live_filesystem
    batch_recover::tests::cancellation_thaws_live_filesystem
)
listing="$(timeout -k 2s 10s "$binary" --list)"
for test_name in "${tests[@]}"; do
    grep -Fxq "$test_name: test" <<<"$listing" || {
        echo "Not the expected library-test executable: missing $test_name" >&2
        exit 2
    }
done

fixture="$(mktemp -d /tmp/rotshield-live-freeze.XXXXXX)"
loop=''
pointer_created=0
cleanup() {
    local rc=$?
    trap - EXIT INT TERM
    if mountpoint -q "$fixture/mnt"; then
        # Only our freshly created mount; failure can mean already thawed.
        timeout -k 2s 5s fsfreeze -u "$fixture/mnt" >/dev/null 2>&1 || :
        if ! timeout -k 2s 10s umount "$fixture/mnt"; then
            echo "Cleanup needs manual inspection; preserving $fixture and $loop" >&2
            exit 1
        fi
    fi
    if [ -n "$loop" ] && ! losetup -d "$loop"; then
        echo "Loop detach failed; preserving $fixture and fixture pointer." >&2
        exit 1
    fi
    if [ "$pointer_created" = 1 ] && [ ! -L "$pointer" ] &&
        [ "$(cat "$pointer")" = "$fixture" ]; then
        rm -f "$pointer"
    fi
    rm -rf "$fixture"
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$fixture/mnt"
truncate -s 128M "$fixture/data.img"
# Format ONLY the new regular file, before attaching it to an available loop.
mkfs.btrfs -q "$fixture/data.img"
loop="$(losetup --find --show "$fixture/data.img")"
mount -t btrfs -o nodiscard "$loop" "$fixture/mnt"
(
    umask 077
    set -o noclobber
    printf '%s\n' "$fixture" >"$pointer"
)
pointer_created=1
printf 'Disposable fixture: %s (%s)\n' "$fixture" "$loop"

for test_name in "${tests[@]}"; do
    printf '\nRunning %s\n' "$test_name"
    timeout -k 5s 30s "$binary" --ignored --exact "$test_name" --nocapture --test-threads=1
done
printf '\nPASS: existing live freeze, write-failure thaw, and cancellation-thaw tests\n'
