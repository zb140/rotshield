#!/bin/bash
# scripts/scrub.sh — backend runner for the Rotshield plugin.
#
#   scrub.sh run      run scrub-rs against the configured device(s). Multiple
#                     disks (DEVICES, space-separated) are scrubbed SEQUENTIALLY
#                     — one after another, in the order listed in DEVICES (the
#                     Settings page stores them alphabetically). The full output
#                     is appended to /var/log/$PLUGIN.log and mirrored to a
#                     per-run file ($CONFIG_DIR/runs/run-<ts>.log; rotation keeps
#                     KEEP_RUNS). Each device's section ends with its `status:`
#                     block (exact final counters), which status.php reads back.
#                     A run lock serialises manual and scheduled runs — if one
#                     is already active, the second invocation skips.
#   scrub.sh running  print 1 if a scrub is currently running, else 0.
#   scrub.sh devices  print the array's data-disk raw rdevs.
#   scrub.sh fstypes  print "<device> <fstype>" per array data disk (btrfs
#                     vs other), read from lsblk's udev-database table —
#                     zero disk I/O, so the Settings page can never spin up
#                     a spun-down drive. Backs the page's grey-out of
#                     non-btrfs disks.
#   scrub.sh stop     terminate a running scrub (kills the runner + scrub-rs).
#                     Before killing, one snapshot of the live counters is
#                     fetched and recorded in the run log as a `status:`
#                     block with state=cancelled, so the UI keeps the aborted
#                     disk's last known numbers.
#
# Both the "Run Scrub Now" button and the scheduled cron job call `run`, so
# manual and scheduled runs are logged identically and show up the same way
# in the UI.

set -u
PLUGIN="rotshield"
PLUGIN_DIR="${ROTSHIELD_PLUGIN_DIR:-${PLUGIN_DIR:-/usr/local/emhttp/plugins/${PLUGIN}}}"
CONFIG_DIR="${ROTSHIELD_CONFIG_DIR:-/boot/config/plugins/${PLUGIN}}"
CONFIG_FILE="${CONFIG_DIR}/config.cfg"
LOG="${ROTSHIELD_LOG:-${LOG:-/var/log/${PLUGIN}.log}}"
RUNS_DIR="${CONFIG_DIR}/runs"
# Run lock: an atomic mkdir-based mutex that (a) serialises manual vs
# scheduled runs and (b) backs the `running` state check. `pid` inside the
# lock records the owning process so stale locks (crash / kill -9) can be
# detected and reclaimed.
LOCK_DIR="${RUNS_DIR}/.lock"
SCRUB="${PLUGIN_DIR}/bin/scrub-rs"
KEEP_RUNS=3

# Script-level flag: set when any device's scrub-rs run did NOT complete
# (failed to print its "scrub complete:" marker), so run() can surface the
# overall run as ERROR rather than trusting a misleading per-device rc.
# Kept at script scope (not a run()-local) so scrub_one_device can set it
# from any calling context without relying on bash dynamic scoping.
errored=0

# Config is sourced dynamically (key=value, both bash and PHP INI parsable).
# shellcheck disable=SC1090
[ -f "${CONFIG_FILE}" ] && . "${CONFIG_FILE}"

# Build the scrub-rs argument string from the structured config keys
# (DEVICE is positional and handled separately). The partition offset is
# auto-applied per device in run() via offset_for() (--offset +<rdevOffset>
# from /proc/nmdstat), so it is NOT part of build_args. The freeze mount is
# also auto-detected per device (see run()), so it is not here either.
# Recovery assessment is ALWAYS on (free + read-only); --repair is what
# opts in to writing reconstructed blocks back. Dry-run is the safe default,
# so we pass --repair ONLY when WRITE is explicitly enabled.
#
# The live status server is ON by default: scrub-rs serves the live error
# counters over a root-only Unix socket (default
# /var/run/rotshield/status.sock, mode 0600) so the Settings page can poll
# the running scrub's error counters via `scrub.sh status`. No TCP port is
# consumed and there is no config knob; a stale socket file left by a
# crashed run is unlinked and rebound by scrub-rs, and the path can be
# overridden with `--status-sock` via EXTRA_OPTIONS.
#
# Plugin-owned policy cannot be overridden by extra options. Values are plain
# whitespace-delimited tokens, never evaluated as shell syntax or glob patterns.
SCRUB_ARGS=()
STATUS_SOCKET=/var/run/rotshield/status.sock
safety_pending=0
repair_aborted=0

build_args() {
    SCRUB_ARGS=()
    STATUS_SOCKET=/var/run/rotshield/status.sock
    case "${WRITE-0}" in 0 | 1) ;; *)
        echo 'REFUSED: WRITE must be 0 or 1' >&2
        return 2
        ;;
    esac
    local extra="${EXTRA_OPTIONS:-}" key value token
    local -a tokens=() tuning=()
    local -A seen=()
    if [[ "$extra" == *$'\n'* || "$extra" == *$'\r'* ]]; then
        echo 'REFUSED: extra options must be on one line' >&2
        return 2
    fi
    read -r -a tokens <<<"$extra"
    set -- "${tokens[@]}"
    while [ "$#" -gt 0 ]; do
        token="$1"
        shift
        key="${token%%=*}"
        case "$key" in
        --status-sock | --batch-max | --batch-idle | --freeze-max) ;;
        *)
            echo "REFUSED: plugin does not allow extra option $key" >&2
            return 2
            ;;
        esac
        if [ -n "${seen[$key]:-}" ]; then
            echo "REFUSED: duplicate extra option $key" >&2
            return 2
        fi
        seen[$key]=1
        if [[ "$token" == *=* ]]; then
            value="${token#*=}"
        else
            [ "$#" -gt 0 ] || {
                echo "REFUSED: $key needs a value" >&2
                return 2
            }
            value="$1"
            shift
        fi
        case "$key" in
        --status-sock)
            [[ "$value" == /* && "$value" != / ]] || return 2
            STATUS_SOCKET="$value"
            ;;
        --batch-max)
            [[ "$value" =~ ^[0-9]+$ && "$value" =~ [1-9] && ${#value} -le 9 ]] || return 2
            tuning+=("$key" "$value")
            ;;
        --batch-idle | --freeze-max)
            [[ "$value" =~ ^[0-9]+([.][0-9]+)?$ && ${#value} -le 12 ]] || return 2
            if [ "$key" = --freeze-max ] && [[ ! "$value" =~ [1-9] ]]; then return 2; fi
            tuning+=("$key" "$value")
            ;;
        esac
    done
    [ "${WRITE:-0}" = 1 ] && SCRUB_ARGS+=(--repair)
    SCRUB_ARGS+=(--status-sock "$STATUS_SOCKET" "${tuning[@]}")
    return 0
}

# Retain the existing stop-pending interlock for any uncertain thaw/repair exit.
# A failed thaw may later succeed on retry; the marker still requires inspection.
retain_safety_lock() {
    safety_pending=1
    printf '%s\n' "$1" >"${LOCK_DIR}/safety-reason"
    : >"${LOCK_DIR}/stop-pending"
}

# Resolve an array data-disk slot from its raw rdev path.
# Uses the same proc source precedence as data_devices(), including
# PROC_NMDSTAT for synthetic integration tests.
array_slot_for() {
    local want="$1" stat p slot name

    for p in "${PROC_NMDSTAT:-}" /proc/nmdstat /proc/mdstat; do
        [ -n "$p" ] && [ -f "$p" ] && {
            stat="$p"
            break
        }
    done
    [ -n "$stat" ] || return 1

    while IFS='=' read -r key val; do
        case "$key" in
        rdevName.*)
            slot="${key#rdevName.}"
            [ "$slot" = "0" ] && continue
            [ "$slot" = "29" ] && continue

            name="$(echo "$val" | xargs)"
            [ -n "$name" ] || continue
            case "$name" in
            /*) ;;
            *) name="/dev/$name" ;;
            esac

            if [ "$name" = "$want" ]; then
                echo "$slot"
                return 0
            fi
            ;;
        esac
    done <"$stat"

    return 1
}

# Resolve the persistent physical disk identity from the array table.
# Never fall back to a transient /dev/sdX name.
disk_id_for() {
    local device="$1" slot stat p

    slot="$(array_slot_for "$device")"
    [ -n "$slot" ] || return 1

    for p in "${PROC_NMDSTAT:-}" /proc/nmdstat /proc/mdstat; do
        if [ -n "$p" ] && [ -f "$p" ]; then
            stat="$p"
            break
        fi
    done

    [ -n "${stat:-}" ] || return 1

    awk -F= -v s="$slot" '
        $1 == "rdevId." s {
            id = substr($0, index($0, "=") + 1)
            sub(/^[[:space:]]+/, "", id)
            sub(/[[:space:]]+$/, "", id)
            if (id != "") {
                print id
                found = 1
            }
            exit
        }
        END { exit !found }
    ' "$stat"
}

# Resolve the live filesystem mountpoint for a raw array member.
# Used to supply --freeze-mount during recovery writes.
# Returns an empty result if no mountpoint can be resolved.
#
# The raw rdev being scrubbed is not necessarily the device through which
# its filesystem is mounted. On Unraid, for example:
#
#   /dev/sdl -> array slot 6 -> /dev/md6p1 -> /mnt/disk6
#
# NonRAID uses /dev/nmd<N>p<M> for its array devices.
#
# Resolution order:
#   1. Map the raw rdev to its array slot using /proc/mdstat or
#      /proc/nmdstat, then check the corresponding array partition.
#   2. Check partition children of the raw device.
#   3. Check the raw device itself.
#   4. Fall back to /proc/self/mountinfo major:minor matching.
#
# The major:minor fallback is not reliable for all filesystems:
# Btrfs may report an anonymous filesystem device number in mountinfo.
#
# IMPORTANT: An empty result does NOT prove that the filesystem is
# unmounted. Plugin-managed repair must refuse to proceed without a
# resolved freeze mountpoint.
freeze_mount_for() {
    local dev="$1" child target slot mddev

    # Array filesystems are mounted through the md/nmd layer, not
    # necessarily through the raw rdev or its physical partitions.
    slot="$(array_slot_for "$dev")"
    if [ -n "$slot" ]; then
        for mddev in "/dev/md${slot}p1" "/dev/nmd${slot}p1"; do
            [ -b "$mddev" ] || continue

            if command -v findmnt >/dev/null 2>&1; then
                target="$(findmnt -n -o TARGET -S "$mddev" 2>/dev/null | head -1)"
                [ -n "$target" ] && {
                    echo "$target"
                    return 0
                }
            fi

            target="$(mountinfo_target_for "$mddev")"
            [ -n "$target" ] && {
                echo "$target"
                return 0
            }
        done
    fi

    if command -v findmnt >/dev/null 2>&1; then
        # Physical partition children, for non-md-backed mount layouts.
        for child in $(device_children "$dev"); do
            target="$(findmnt -n -o TARGET -S "$child" 2>/dev/null | head -1)"
            [ -n "$target" ] && {
                echo "$target"
                return 0
            }
        done
        # Whole-device mounts and directly targeted array partitions.
        target="$(findmnt -n -o TARGET -S "$dev" 2>/dev/null | head -1)"
        [ -n "$target" ] && {
            echo "$target"
            return 0
        }
    fi
    # major:minor containment — the robust backstop (also the only path
    # when findmnt is missing).
    target="$(mountinfo_target_for "$dev")"
    [ -n "$target" ] && echo "$target"
    return 0
}

# List the partition children of a block device, one absolute path per
# line.  Prefers lsblk's udev-database table (zero disk I/O — the same
# call `fstypes` makes, so a spun-down drive is never spun up); falls back
# to sysfs when lsblk is unavailable.
device_children() {
    local dev="$1" base="${1##*/}" part out
    if command -v lsblk >/dev/null 2>&1; then
        out="$(lsblk -p -rno NAME,PKNAME 2>/dev/null | awk -v d="$dev" '$2==d {print $1}')"
        [ -n "${out}" ] && {
            printf '%s\n' "${out}"
            return 0
        }
        # lsblk present but produced nothing (broken/stale udev DB, chroot,
        # device without a udev record): fall through to the sysfs
        # enumeration below rather than silently returning an empty set.
    fi
    # sysfs fallback: partition dirs are named <parent>p<N> (loop, md) or
    # <parent><N> (sdX).  Unmatched globs stay literal and fail the `-d`.
    for part in /sys/class/block/"${base}"/"${base}"p[0-9]* \
        /sys/class/block/"${base}"/"${base}"[0-9]*; do
        [ -d "$part" ] && echo "/dev/${part##*/}"
    done
    return 0
}

# Echo the mount target of the first /proc/self/mountinfo entry whose
# device major:minor matches the given device or one of its partition
# children.  Matching by device number — never by source string — finds
# mounts registered by path, by UUID, or by label alike.  Returns nothing
# (exit 1) when nothing is mounted.
mountinfo_target_for() {
    local dev="$1" real sys_dev sys_child target child
    real="$(readlink -f "$dev" 2>/dev/null)"
    [ -n "$real" ] || real="$dev"
    sys_dev="/sys/class/block/${real##*/}/dev"
    # Children first (their filesystem is the one being scrubbed), then the
    # device itself — mirrors freeze_mount_for's preference.
    for child in $(device_children "$dev"); do
        sys_child="/sys/class/block/${child##*/}/dev"
        [ -f "$sys_child" ] || continue
        target="$(mountinfo_target_for_majmin "$(cat "$sys_child" 2>/dev/null)")"
        [ -n "$target" ] && {
            echo "$target"
            return 0
        }
    done
    [ -f "$sys_dev" ] || return 1
    target="$(mountinfo_target_for_majmin "$(cat "$sys_dev" 2>/dev/null)")"
    [ -n "$target" ] && {
        echo "$target"
        return 0
    }
    return 1
}

# Echo the mount target of the first mountinfo entry with device
# major:minor `want`, decoding mountinfo's \NNN octal escapes for the
# common characters (space, tab, newline, backslash).
mountinfo_target_for_majmin() {
    local want="$1" target
    [ -n "$want" ] || return 0
    target="$(awk -v w="$want" '$3==w {print $5; exit}' /proc/self/mountinfo 2>/dev/null)"
    [ -n "$target" ] || return 0
    printf '%s' "$target" | sed 's/\\040/ /g; s/\\011/\t/g; s/\\012/\n/g; s/\\134/\\/g'
}

# Shared, bounded delivery and summaries; no scanner/write policy lives here.
. "${PLUGIN_DIR}/scripts/notify-common.sh"

recovery_note_for_log() {
    local device_log="$1"
    local recovered

    recovered="$(awk -F: '/^  recovered[[:space:]]*:/ {gsub(/^[[:space:]]+/, "", $2); print $2; exit}' "$device_log")"
    [ -z "$recovered" ] && return 0

    if [ "${WRITE:-0}" = "1" ]; then
        echo "; recovered ${recovered}"
    else
        echo "; recoverable ${recovered}"
    fi
}

# Atomically claim the run lock (mkdir is atomic, so exactly one process
# wins). Serialises manual + scheduled runs: a second invocation — e.g. the
# cron job firing while a manual run is active — finds the lock held by a
# live process and skips instead of starting a duplicate scrub of the same
# disk. Stale locks (crash / kill -9) are reclaimed. On success an EXIT trap
# releases the lock on every exit path.
acquire_lock() {
    if mkdir "${LOCK_DIR}" 2>/dev/null; then
        echo "$$" >"${LOCK_DIR}/pid"
        trap '[ -e "${LOCK_DIR}/stop-pending" ] || rm -rf "${LOCK_DIR}" 2>/dev/null' EXIT
        return 0
    fi
    # A failed stop may leave a scanner alive after its runner exits.
    # Never reclaim that lock automatically; require operator verification.
    if [ -e "${LOCK_DIR}/stop-pending" ]; then
        echo "Previous stop is unresolved; refusing to start another scrub. Inspect the process and mount before clearing ${LOCK_DIR}."
        return 1
    fi
    local opid=""
    [ -f "${LOCK_DIR}/pid" ] && opid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
    if [ -n "${opid}" ] && kill -0 "${opid}" 2>/dev/null; then
        echo "A scrub is already running (pid ${opid}). Skipping."
        return 1
    fi
    # Lock dir exists but its owner is dead — reclaim it.
    rm -rf "${LOCK_DIR}"
    if mkdir "${LOCK_DIR}" 2>/dev/null; then
        echo "$$" >"${LOCK_DIR}/pid"
        trap '[ -e "${LOCK_DIR}/stop-pending" ] || rm -rf "${LOCK_DIR}" 2>/dev/null' EXIT
        return 0
    fi
    echo "Could not acquire run lock. Skipping."
    return 1
}

# Map a device's exit code to its status label. `dev_log` is that device's
# captured output (used for the recovered/recoverable note). scrub-rs exit
# code contract (mode-independent — same disk => same code regardless of
# flags): 0 clean | 1 runtime/setup error | 2 usage error |
# 3 issues (plain) | 4 all recoverable | 5 some unrecoverable | 6 metadata
# fatal (offline `btrfs check --repair`).
device_status() {
    local rc="$1" dev_log="$2" note=""
    note="$(recovery_note_for_log "${dev_log}")"
    case "${rc}" in
    0) echo "OK (clean)${note}" ;;
    3) echo "ISSUES FOUND${note}" ;;
    4) echo "ISSUES FOUND - all recoverable${note}" ;;
    5) echo "ISSUES FOUND - some UNRECOVERABLE${note}" ;;
    6) echo "METADATA FATAL${note}" ;;
    *) echo "ERROR (rc=${rc})${note}" ;;
    esac
}

# Scrub one device: log its progress and return scrub-rs's exit code. scrub-rs
# output is streamed live to the run log (tee) while captured in a per-device
# temp log for status parsing, so "View Logs" shows live progress.
scrub_one_device() {
    local device="$1" idx="$2" total="$3"
    shift 3
    local -a dev_opts=("$@")
    local off fm rc status dev_log
    local disk_id="" disk_slot="" started_at="" outcome="error"

    # Target the RAW rdev: scrub-rs reads the btrfs superblock at the partition
    # offset, so we pass --offset +<rdevOffset> (sectors) from /proc/nmdstat. If
    # the offset is wrong it rejects the device early with a clear superblock
    # error — it never silently scrubs garbage. An array partition
    # (/dev/nmd<N>p<M>) is the one exception: its btrfs superblock lives at
    # offset 0 (the array driver already strips the per-disk header), so no
    # --offset is passed and no warning is warranted.
    off="$(offset_for "${device}")"
    if [ -n "${off}" ]; then
        dev_opts+=(--offset "+${off}")
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: partition offset +${off} sectors"
    elif is_array_partition "${device}"; then
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: array partition (superblock at offset 0)"
    else
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: WARNING no rdevOffset found in nmdstat; scrub-rs will likely reject this device"
    fi
    # Plugin repair always requires the discovered live freeze mount. Offline
    # repair and explicit freeze bypasses belong to the standalone CLI only.
    fm="$(freeze_mount_for "${device}")"
    if [ -n "$fm" ]; then
        dev_opts+=(--freeze-mount "$fm")
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: freeze mount auto-detected at $fm"
    elif [ "${WRITE:-0}" = 1 ]; then
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: REFUSED: repair requires a resolved filesystem mountpoint"
        errored=1
        repair_aborted=1
        return 1
    fi

    # Serialize launch with Stop. A dedicated FIFO keeps the collector alive
    # until Rust has emitted its post-join thaw/cleanup records.
    local dev_tmp scanner_pid collector_pid launch_fd collector_rc
    dev_tmp="$(mktemp -d /tmp/rotshield-scan.XXXXXX)" || return 1
    dev_log="$dev_tmp/output"
    mkfifo "$dev_tmp/stream" || {
        rmdir "$dev_tmp"
        return 1
    }
    exec {launch_fd}>"${RUNS_DIR}/.launch.lock"
    flock -x "$launch_fd" || {
        exec {launch_fd}>&-
        rm -rf "$dev_tmp"
        return 1
    }
    if [ -e "${LOCK_DIR}/stop-pending" ]; then
        exec {launch_fd}>&-
        rm -rf "$dev_tmp"
        return 1
    fi
    disk_id="$(disk_id_for "$device")"
    disk_slot="$(array_slot_for "$device")"
    started_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

    if [ -z "$disk_id" ] || [ -z "$disk_slot" ]; then
        echo "WARNING: Cannot resolve disk identity for $device; result will not be persisted"
    fi
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) scrubbing ${device}"
    rm -f "${LOCK_DIR}/disk-finalized"
    printf '%s\n' "$device" >"${LOCK_DIR}/active-device"
    printf '%s\n' "$disk_id" >"${LOCK_DIR}/disk-id"
    printf '%s\n' "$disk_slot" >"${LOCK_DIR}/disk-slot"
    printf '%s\n' "$started_at" >"${LOCK_DIR}/disk-started-at"
    printf '%s\n' "$fm" >"${LOCK_DIR}/freeze-mount"
    printf '%s\n' "$dev_tmp" >"${LOCK_DIR}/scanner-temp"
    (
        exec {launch_fd}>&-
        exec tee "$dev_log" <"$dev_tmp/stream"
    ) &
    collector_pid=$!
    (
        exec {launch_fd}>&-
        exec "$SCRUB" "$device" "${dev_opts[@]}" >"$dev_tmp/stream" 2>&1
    ) &
    scanner_pid=$!
    printf '%s\n' "$scanner_pid" >"${LOCK_DIR}/scanner-pid"
    exec {launch_fd}>&-
    wait "$scanner_pid"
    rc=$?
    wait "$collector_pid"
    collector_rc=$?

    # Serialize disk finalization with Stop.
    exec {launch_fd}>"${RUNS_DIR}/.launch.lock"
    flock -x "$launch_fd" || {
        exec {launch_fd}>&-
        rm -rf "$dev_tmp"
        errored=1
        repair_aborted=1
        return 1
    }

    # Stop owns the partial snapshot and terminal record once pending is set.
    # Rust output has drained, so Stop can inspect its final safety markers.
    if [ -e "${LOCK_DIR}/stop-pending" ]; then
        exec {launch_fd}>&-
        rm -rf "$dev_tmp"
        return "$rc"
    fi

    # Only trust the rc→status mapping when scrub-rs actually ran to completion
    # (printed its completion marker). Otherwise the exit code isn't a reliable
    # status — the tool aborted early (bad args, unopenable device, panic,
    # signal) — so report an error and flag the run so the finished: line is
    # ERROR rather than a misleading ISSUES/OK label.
    local advisory=""
    if grep -q '^repaired_while_mounted=1$' "${dev_log}"; then
        # C5: the repair wrote under a live mount.  BLKFLSBUF on the raw rdev
        # clears the block-device buffer cache, but the mounted filesystem's
        # FILE page cache may still serve the OLD corrupt bytes for ranges that
        # were cached before the repair — advise a reboot/remount so the
        # operator does not read stale (still-corrupt) data after a reported
        # "successful" fix.
        advisory=" (repaired under live mount - reboot/remount recommended)"
    fi
    if grep -q '^scrub complete:' "$dev_log"; then
        status="$(device_status "$rc" "$dev_log")${advisory}"
    else
        status="ERROR (rc=$rc; incomplete scanner output)"
        errored=1
        if [ "${WRITE:-0}" = 1 ] && [ ! -e "${LOCK_DIR}/stop-pending" ] &&
            ! grep -qx 'repair_cleanup=complete' "$dev_log"; then
            retain_safety_lock 'Repair scanner did not complete; verify filesystem thaw before clearing lock'
        fi
        [ "$rc" -ne 0 ] || rc=1
    fi
    if grep -qx 'thaw_failed=1' "$dev_log"; then
        retain_safety_lock 'A filesystem thaw failed; verify thaw manually before clearing lock'
        status="ERROR (filesystem thaw requires manual verification)"
        errored=1
        rc=1
    fi
    if [ "$collector_rc" -ne 0 ]; then
        errored=1
        rc=1
        if [ "${WRITE:-0}" = 1 ]; then
            retain_safety_lock 'Repair log collector failed; cleanup evidence may be incomplete'
        fi
    fi
    if [ "${WRITE:-0}" = 1 ]; then
        case "$rc" in 0 | 3 | 4 | 5) ;; *) repair_aborted=1 ;; esac
    fi
    [ "$safety_pending" = 0 ] || repair_aborted=1
    if [ "$collector_rc" -eq 0 ] &&
        [ "$safety_pending" -eq 0 ] &&
        grep -q '^scrub complete:' "$dev_log"; then
        case "$rc" in
        0) outcome="clean" ;;
        3) outcome="issues" ;;
        4) outcome="recoverable" ;;
        5) outcome="unrecoverable" ;;
        6) outcome="metadata_fatal" ;;
        *) outcome="error" ;;
        esac
    fi

    if [ -n "$disk_id" ] && [ -n "$disk_slot" ]; then
        php "${PLUGIN_DIR}/scripts/results.php" \
            "${CONFIG_DIR}/results.json" \
            "$disk_id" \
            "$device" \
            "$disk_slot" \
            "$started_at" \
            "$outcome" \
            "$rc" \
            "${WRITE:-0}" \
            "$dev_log" ||
            echo "WARNING: Failed to persist result for $device"
    fi

    rm -rf "$dev_tmp"
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ${status}"
    : >"${LOCK_DIR}/disk-finalized"
    exec {launch_fd}>&-

    return "${rc}"
}

run() {
    mkdir -p "${RUNS_DIR}"
    acquire_lock || return 0

    # Resolve the device list: DEVICES (space-separated, multiple disks) takes
    # precedence; fall back to the single DEVICE key for backwards compat.
    # The order here IS the scrub order — config is the single source of truth
    # (the Settings page stores DEVICES alphabetically, but a hand-edited
    # config is honored verbatim).  An empty (deliberately saved) target list
    # means "don't scrub": never invent a target.  (The old /dev/nmd1p1
    # last-resort pointed at a device that is not a discoverable option on
    # every system.)
    local devlist="${DEVICES:-${DEVICE:-}}"
    if [ -z "${devlist}" ]; then
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] no disks targeted (empty DEVICES) — nothing to scrub" >>"${LOG}"
        return 1
    fi
    local total
    total=$(echo ${devlist} | wc -w)
    build_args || {
        echo "REFUSED: invalid runner policy/options" >>"$LOG"
        return 2
    }
    local opts
    printf -v opts '%q ' "${SCRUB_ARGS[@]}"
    local start_ts
    start_ts="$(date '+%Y-%m-%d %H:%M:%S')"
    local run_log
    run_log="${RUNS_DIR}/run-$(date '+%Y%m%d-%H%M%S').log"

    local overall_rc=0 idx=0 rc
    # Publish monitor readiness only after all metadata and the start attempt.
    : >"${run_log}" || return 1
    printf '%s\n' "${run_log}" >"${LOCK_DIR}/run-log"
    printf '%s\n' "$(date +%s)" >"${LOCK_DIR}/start-epoch"
    printf '%s\n' "${total}" >"${LOCK_DIR}/total"
    printf '%s\n' "${WRITE:-0}" >"${LOCK_DIR}/write-mode"
    printf '%s\n' "$(status_sock)" >"${LOCK_DIR}/status-sock"
    notify_job_started "${total}" "${start_ts}"
    : >"${LOCK_DIR}/notify-ready"
    {
        echo "[${start_ts}] starting sequential scrub of: ${devlist} ${opts}"
        for device in ${devlist}; do
            # Stop must not advance to another disk while cancellation is pending.
            if [ -e "${LOCK_DIR}/stop-pending" ]; then
                echo "[$(date '+%Y-%m-%d %H:%M:%S')] cancellation pending; not starting another disk"
                overall_rc=1
                errored=1
                break
            fi
            idx=$((idx + 1))
            scrub_one_device "${device}" "${idx}" "${total}" "${SCRUB_ARGS[@]}"
            rc=$?
            [ "${rc}" -gt "${overall_rc}" ] && overall_rc=${rc}
            if [ "$repair_aborted" = 1 ]; then
                echo 'job_aborted=1'
                echo 'Repair job aborted: partial results; no further disks will be scanned'
                break
            fi
        done
        # Stop owns cancellation and its partial summary once stop-pending exists.
        if [ -e "${LOCK_DIR}/stop-pending" ] && [ "$safety_pending" = 0 ]; then
            return 1
        fi
        notify_quiesce
        local finish_fd
        exec {finish_fd}>"${RUNS_DIR}/.launch.lock"
        flock -x "$finish_fd"
        if [ -e "${LOCK_DIR}/stop-pending" ] && [ "$safety_pending" = 0 ]; then
            exec {finish_fd}>&-
            return 1
        fi
        # If any device's tool aborted, surface the run as an error regardless of
        # the numeric codes.
        if [ "${errored}" -ne 0 ]; then
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ERROR(rc=${overall_rc})"
        else
            # Overall outcome for the run (highest-priority non-clean code wins).
            case "${overall_rc}" in
            0) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: OK" ;;
            6) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: METADATA_FATAL(rc=6)" ;;
            5) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND_UNRECOVERABLE(rc=5)" ;;
            4) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND_RECOVERABLE(rc=4)" ;;
            3) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND(rc=3)" ;;
            *) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ERROR(rc=${overall_rc})" ;;
            esac
        fi
        exec {finish_fd}>&-
    } >"${run_log}"

    notify_job_finished "${run_log}" finished "${overall_rc}"

    # Keep the persistent plugin log simple: append the exact run log bytes.
    cat "${run_log}" >>"${LOG}"

    # Prune old run logs, keep the most recent KEEP_RUNS.  Filenames are
    # tool-generated (run-YYYYMMDD-HHMMSS.log) so `ls -1t` (newest first) is
    # safe here; the glob below is only used to find the candidates.
    local n=0
    # shellcheck disable=SC2045  # controlled, whitespace-free names
    for f in $(ls -1t "${RUNS_DIR}"/run-*.log 2>/dev/null); do
        n=$((n + 1))
        [ "${n}" -gt "${KEEP_RUNS}" ] && rm -f "${f}"
    done
    return "${overall_rc}"
}

# Live check: is a scrub currently running? Backs the page's "Stop Scrub"
# button and status line, and works for BOTH manual and scheduled runs —
# every run() acquires the same lock dir. A stale lock whose owner is dead
# reports 0.
running() {
    [ -d "${LOCK_DIR}" ] || {
        echo "0"
        return 0
    }
    # An unresolved stop is not an idle state, even if the runner has exited.
    # Keep the Stop control/status active until an operator resolves the lock.
    if [ -e "${LOCK_DIR}/stop-pending" ]; then
        echo "1"
        return 0
    fi
    local pid
    pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
    if [ -n "${pid}" ] && kill -0 "${pid}" 2>/dev/null; then
        echo "1"
    else
        echo "0"
    fi
}

# Read the array's data-disk table from /proc/nmdstat (or /proc/mdstat;
# PROC_NMDSTAT overrides for testing). We target the RAW rdev (e.g. /dev/sdX,
# /dev/loopX — whatever the kernel reports in rdevName.N), NOT the array
# partition (/dev/nmdNp1), because scrub-rs needs the raw device to (a) read
# the btrfs superblock at its partition offset and (b) write recovered blocks
# back through the array layer. Parity slots (0=P, 29=Q) are excluded — only
# btrfs data disks apply. Emits "name|offset" lines (name already qualified
# with /dev/ if needed).
data_devices() {
    local stat p
    for p in "${PROC_NMDSTAT:-}" /proc/nmdstat /proc/mdstat; do
        [ -n "$p" ] && [ -f "$p" ] && {
            stat="$p"
            break
        }
    done
    [ -z "$stat" ] && return 0
    local slot name off
    while IFS='=' read -r key val; do
        case "$key" in
        rdevName.*)
            slot="${key#rdevName.}"
            [ "$slot" = "0" ] && continue
            [ "$slot" = "29" ] && continue
            name="$(echo "$val" | xargs)"
            [ -z "$name" ] && continue
            case "$name" in
            /*) ;;
            *) name="/dev/$name" ;;
            esac
            off="$(awk -F= -v s="$slot" '$1=="rdevOffset."s {print $2}' "$stat" | xargs)"
            echo "${name}|${off}"
            ;;
        esac
    done <"$stat"
}

# List the data-disk raw rdev paths (names only, one per line).
devices() {
    data_devices | cut -d'|' -f1
}

# Print "<device> <fstype>" (one line per array data disk) for the Settings
# page, which greys out non-btrfs disks so they can't be selected.  ONE
# lsblk call returns every block device's filesystem straight from the udev
# database — lsblk never opens a disk, so visiting the Settings page cannot
# spin up a spun-down drive (blkid would: it probes the device whenever its
# udev cache misses).  On unRAID the filesystem lives on the data disk's GPT
# partition (/dev/sdc1); the raw rdev (/dev/sdc) itself carries no FSTYPE,
# so each discovered rdev resolves through its direct children: the first
# child row whose PKNAME equals the rdev and whose FSTYPE is non-empty
# wins.  Anything else (xfs, reiserfs, unformatted, no udev record) reports
# "other".  If lsblk is unavailable or produced nothing, fstypes prints
# nothing and the page keeps every disk selectable — scrub-rs re-validates
# the superblock at run time and rejects a non-btrfs device with a clear
# error before scrubbing anything.
fstypes() {
    command -v lsblk >/dev/null 2>&1 || return 0
    local table dev
    # Raw mode (no --list: newer util-linux rejects combining them), no
    # header; rows stay in tree order, parent before its children.
    table="$(lsblk -p -rno FSTYPE,PKNAME 2>/dev/null)"
    [ -n "${table}" ] || return 0
    while read -r dev; do
        local fs
        fs="$(printf '%s\n' "${table}" | awk -v d="${dev}" '$2==d && $1!="" {print $1; exit}')"
        if [ "${fs}" = "btrfs" ]; then
            echo "${dev} btrfs"
        else
            echo "${dev} other"
        fi
    done < <(devices)
}

# True for array-partition device names (/dev/nmd<N>p<M>), where the btrfs
# superblock lives at offset 0 (the array driver strips the per-disk header).
# Mirrors scrub-rs's own slot_from_array_partition pattern (nmd + one-or-more
# digits + p + one-or-more digits). Raw rdevs (/dev/loopN, /dev/sdX) return
# false — they need their rdevOffset applied via --offset.
is_array_partition() {
    local name="${1##*/}"
    case "$name" in
    nmd[0-9]*p[0-9]*) return 0 ;;
    esac
    return 1
}

# Print the partition offset (in 512-byte sectors) for a given raw rdev, or
# nothing if it isn't a known data disk. scrub-rs accepts --offset +N as
# sector multiples.
offset_for() {
    local want="$1"
    data_devices | awk -F'|' -v w="$want" '$1==w {print $2; exit}'
}

# Emergency thaw is deliberately limited to the filesystem belonging to
# this run's last active device. FIFREEZE is not owned by a process: thawing
# every array mount could undo another application's intentional freeze.
# Only call this after the tracked scanner process tree has exited.
thaw_frozen_mounts() {
    local active_dev="$1" mnt
    if [ -z "${active_dev}" ]; then
        echo "WARNING: active scrub device unknown; cannot safely select an emergency thaw mount. Check for frozen filesystems manually."
        return 1
    fi
    if [ -f "${LOCK_DIR}/freeze-mount" ]; then
        mnt="$(cat "${LOCK_DIR}/freeze-mount")"
    else
        mnt="$(freeze_mount_for "$active_dev")"
    fi
    if [ -z "${mnt}" ]; then
        echo "WARNING: no verified mount for ${active_dev}; check for a frozen filesystem manually."
        return 1
    fi
    if ! command -v fsfreeze >/dev/null 2>&1; then
        echo "WARNING: fsfreeze unavailable; if ${mnt} is frozen, thaw it manually."
        return 1
    fi
    if command -v timeout >/dev/null 2>&1 && timeout -k 1s 3s fsfreeze -u "${mnt}" >/dev/null 2>&1; then
        echo "[stop] emergency thaw completed for ${mnt}"
    else
        echo "[stop] ${mnt}: thaw could not be verified; manual inspection required"
        return 1
    fi
}

# Only the current scanner can certify its shutdown. An earlier disk may have
# completed cleanly even when the active disk crashes without cleanup evidence.
last_scanner_has_marker() {
    awk -v wanted="$1" '
        /^\[[^]]+\] \([0-9]+\/[0-9]+\) scrubbing \// { found=0 }
        $0 == wanted { found=1 }
        END { exit !found }
    ' "$2"
}

# Capture descendants before signalling: a parent can exit before its child,
# making a later pgrep -P traversal miss the surviving scanner.
collect_tree() {
    local root="$1" child
    for child in $(pgrep -P "${root}" 2>/dev/null); do
        collect_tree "${child}"
    done
    echo "${root}"
}

# Check tracked processes (including orphaned descendants). A zombie is no
# longer executing repair code, and should not block shutdown completion.
tracked_alive() {
    local pid state
    for pid in "$@"; do
        [ -r "/proc/${pid}/stat" ] || continue
        state="$(awk '{print $3}' "/proc/${pid}/stat" 2>/dev/null)"
        case "${state}" in
        Z | X | '') ;;
        *) return 0 ;;
        esac
    done
    return 1
}

stop() {
    # Optional fallback snapshot file (written by the Settings page with the
    # last live payload it received): used only when the direct status curl
    # comes up empty, so a busy port / dead server can't lose the counters.
    local fallback_file="${1:-}"
    [ -d "${LOCK_DIR}" ] || {
        echo "No scrub is currently running."
        return 0
    }
    local pid
    pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
    local stop_write
    stop_write="$(cat "${LOCK_DIR}/write-mode" 2>/dev/null)"
    [ -n "$stop_write" ] || stop_write="${WRITE:-0}"

    if [ -e "${LOCK_DIR}/stop-pending" ]; then
        echo "WARNING: a previous stop remains unresolved; inspect the scanner and filesystem before clearing the lock."
        return 1
    fi
    if [ -z "${pid}" ] || ! kill -0 "${pid}" 2>/dev/null; then
        # Stale lock — nothing actually running. Clean it up.
        rm -rf "${LOCK_DIR}"
        echo "No scrub is currently running (stale lock removed)."
        return 0
    fi

    local run_log
    run_log="$(newest_run_log)"
    # CHECK: has this run already written its `finished:` line?  run() writes
    # it BEFORE releasing the lock, so a stop in that window must NOT kill the
    # (already done) process or append a fake cancellation after a real
    # completion — that would flip the outcome to CANCELLED and, worse, an
    # empty-counters block would overwrite the device's real final data.
    if [ -n "${run_log}" ] && grep -q "finished:" "${run_log}"; then
        # The runner still owns final notification and log cleanup.
        echo "Run already finished — nothing to cancel."
        return 0
    fi

    # Publish Stop under the same gate as scanner launch and completion. No
    # scanner can appear after the descendant snapshot or advance to a new disk.
    local stop_fd scanner_pid active_dev payload
    local -a tracked=() scanner_tree=()
    exec {stop_fd}>"${RUNS_DIR}/.launch.lock"
    if ! flock -w 15 "$stop_fd"; then
        exec {stop_fd}>&-
        echo 'ERROR: scanner launch/completion gate busy; no process was signalled.'
        return 1
    fi
    if [ -n "$run_log" ] && grep -q 'finished:' "$run_log"; then
        exec {stop_fd}>&-
        echo 'Run already finished — nothing to cancel.'
        return 0
    fi
    if [ -e "${LOCK_DIR}/stop-pending" ] || ! : >"${LOCK_DIR}/stop-pending"; then
        exec {stop_fd}>&-
        echo 'ERROR: stop already pending or cannot publish cancellation.'
        return 1
    fi

    local disk_id="" disk_slot="" started_at=""
    active_dev=""
    scanner_pid=""

    if [ ! -e "${LOCK_DIR}/disk-finalized" ]; then
        active_dev="$(cat "${LOCK_DIR}/active-device" 2>/dev/null)"
        scanner_pid="$(cat "${LOCK_DIR}/scanner-pid" 2>/dev/null)"
        disk_id="$(cat "${LOCK_DIR}/disk-id" 2>/dev/null)"
        disk_slot="$(cat "${LOCK_DIR}/disk-slot" 2>/dev/null)"
        started_at="$(cat "${LOCK_DIR}/disk-started-at" 2>/dev/null)"
    fi

    mapfile -t tracked < <(collect_tree "$pid")
    if [[ "$scanner_pid" =~ ^[0-9]+$ ]] && tracked_alive "$scanner_pid"; then
        mapfile -t scanner_tree < <(collect_tree "$scanner_pid")
    fi
    exec {stop_fd}>&-
    payload="$(status)"
    payload="$(sanitize_status_payload "$payload" "$active_dev")"
    if [ -z "$payload" ] && [ -n "$fallback_file" ] && [ -f "$fallback_file" ]; then
        payload="$(sanitize_status_payload "$(cat "$fallback_file" 2>/dev/null)" "$active_dev")"
    fi
    [ -n "$fallback_file" ] && rm -f "$fallback_file"

    local child waited=0 killed=1 escalated=0
    # Leave the runner and tee alive to drain Rust's shutdown safety records.
    for child in "${scanner_tree[@]}"; do
        kill -TERM "$child" 2>/dev/null || :
    done

    # Allow up to 10 seconds for cooperative cancellation and thaw.
    while [ "${waited}" -lt 20 ] && tracked_alive "${tracked[@]}"; do
        sleep 0.5
        waited=$((waited + 1))
    done
    if tracked_alive "${tracked[@]}"; then
        escalated=1
        echo "WARNING: graceful scrub stop timed out; escalating to SIGKILL."
        for child in "${tracked[@]}"; do
            kill -KILL "${child}" 2>/dev/null || :
        done
        sleep 0.5
    fi
    if tracked_alive "${tracked[@]}"; then
        killed=0
        echo "WARNING: a tracked scrub process is still running; refusing emergency thaw and lock removal."
    elif [ "${escalated}" -eq 1 ] && [ "$stop_write" != 0 ] && [ -n "$scanner_pid" ] &&
        { ! last_scanner_has_marker 'repair_cleanup=complete' "$run_log" ||
            last_scanner_has_marker 'thaw_failed=1' "$run_log"; }; then
        # A SIGKILL cannot run Rust cleanup. Only attempt emergency thaw after
        # forced termination, and only for this run's identified filesystem.
        if ! thaw_frozen_mounts "${active_dev}"; then
            retain_safety_lock 'Forced repair shutdown: emergency thaw unverified; inspect filesystem before clearing lock'
            echo "WARNING: emergency thaw could not be verified; lock retained."
        fi
    fi

    if [ "${killed}" -ne 1 ]; then
        echo "WARNING: scrub process tree did not exit; no final cancellation record written. Lock retained for manual investigation."
        return 1
    fi

    if [ "$stop_write" != 0 ] && [ "$escalated" = 0 ] &&
        { [ -z "$run_log" ] || ! last_scanner_has_marker 'repair_cleanup=complete' "$run_log"; }; then
        # If no scanner was ever launched, there was no freeze to release.
        if [ -n "$scanner_pid" ]; then
            retain_safety_lock 'Repair shutdown cleanup was not confirmed; inspect filesystem before clearing lock'
        fi
    fi
    # Rust may report a thaw failure while handling TERM, after the live
    # snapshot was captured. Treat that sticky record as a safety interlock.
    if [ -n "$run_log" ] && last_scanner_has_marker 'thaw_failed=1' "$run_log"; then
        retain_safety_lock 'Scanner reported failed thaw during shutdown; manual verification required'
    fi

    if [ -n "$disk_id" ] && [ -n "$disk_slot" ] &&
        [ -n "$started_at" ] && [ -n "$active_dev" ]; then
        local outcome="cancelled"
        [ "$safety_pending" = 0 ] || outcome="safety_abort"

        php "${PLUGIN_DIR}/scripts/results.php" \
            "${CONFIG_DIR}/results.json" \
            "$disk_id" \
            "$active_dev" \
            "$disk_slot" \
            "$started_at" \
            "$outcome" \
            1 \
            "$stop_write" \
            php://stdin <<EOF
status:
$(printf '%s\n' "$payload" | sed '/^state=/d; /^device=/d; /^$/d')
state=cancelled
device=${active_dev}
EOF
    fi

    notify_quiesce
    if [ "$safety_pending" = 1 ]; then
        if [ -n "$run_log" ]; then
            {
                echo
                echo 'job_aborted=1'
                echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: SAFETY_ABORT (shutdown thaw unverified)"
            } >>"$run_log"
        fi
        notify_job_finished "$run_log" aborted 1
        echo 'Scrub processes stopped, but filesystem safety is unresolved; lock retained.'
        return 1
    fi
    # Record the cancellation in the run log (after the tree is dead, so this
    # block is the last thing in the file).  status.php parses it like any
    # other final block; the UI shows the aborted disk as cancelled with the
    # counters captured above.
    if [ -n "${run_log}" ]; then
        # Safe: read the final byte before appending.
        # shellcheck disable=SC2094
        {
            # Newline guard: if the killed process left the log ending mid-line,
            # start the append on a fresh line so the `status:` marker stays a
            # standalone line and the block parses.
            tail -c 1 "${run_log}" 2>/dev/null | grep -q . && echo
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] manual stop requested"
            if [ -n "$active_dev" ]; then
                echo "status:"
                if [ -n "$payload" ]; then
                    echo "$payload" | sed 's/^state=.*/state=cancelled/' | sed '/^$/d'
                else
                    echo "state=cancelled"
                    echo "device=${active_dev}"
                fi
            fi
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: CANCELLED (manual stop)"
        } >>"${run_log}"

        if ! awk -v active="$active_dev" '
            /^status:$/ { state = "" }
            /^state=/ { state = $0 }
            NF { last = $0 }
            END {
                exit !((active == "" || state == "state=cancelled") &&
                       last ~ /finished: CANCELLED \(manual stop\)$/)
            }
        ' "${run_log}"; then
            echo "WARNING: could not record the cancellation in the run log."
        fi
    fi
    # The run() EXIT trap normally clears this; remove a stale lock after
    # verifying that the tracked process tree has stopped.
    notify_job_finished "${run_log}" cancelled 1
    local scanner_temp
    scanner_temp="$(cat "${LOCK_DIR}/scanner-temp" 2>/dev/null)"
    if [[ "$scanner_temp" == /tmp/rotshield-scan.* && ! -L "$scanner_temp" ]]; then
        rm -rf "$scanner_temp"
    fi
    rm -rf "${LOCK_DIR}"
    echo "Scrub stopped."

}

# Validate a candidate status payload: it must carry state= and device=,
# and (when the run log tells us which disk is active) describe that disk.
# Echoes the payload unchanged if valid, nothing otherwise — so a busy port,
# a dead server, a foreign server, or a stale snapshot can never record
# garbage into the run log.
sanitize_status_payload() {
    local cand="$1" expect_dev="$2"
    [ -n "${cand}" ] || return 0
    local pdev pstate
    pdev="$(echo "${cand}" | awk -F= '$1=="device"{print $2; exit}')"
    pstate="$(echo "${cand}" | awk -F= '$1=="state"{print $2; exit}')"
    if [ -z "${pdev}" ] || [ -z "${pstate}" ] ||
        { [ -n "${expect_dev}" ] && [ "${pdev}" != "${expect_dev}" ]; }; then
        return 0
    fi
    echo "${cand}"
}

# Newest per-run log (same listing the rotation uses; names are
# tool-generated run-YYYYMMDD-HHMMSS.log, whitespace-free).
newest_run_log() {
    ls -1t "${RUNS_DIR}"/run-*.log 2>/dev/null | head -1
}

# Device of the run's most recently started scrub (last "scrubbing <dev>"
# line in the log) — the disk that was in flight when a stop killed the run.
# Only the runner's OWN marker line matches: scrub-rs also prints a
# "scrubbing (recovery assessment + dry-run):" banner into the same log, and
# its LAST token is "dry-run):" — matching it handed stop() a bogus device,
# which then failed sanitize_status_payload and silently dropped the
# cancelled block's counters (the UI showed a phantom "dry-run):cancelled"
# column instead of the interrupted disk's numbers).  The runner's marker is
# the only "scrubbing" line whose next token is an absolute device path
# (starts with "/"), so the pattern requires that.
last_scrubbed_device() {
    local log="$1"
    awk '/scrubbing[[:space:]]+\//{dev=$NF} END{print dev}' "${log}" 2>/dev/null
}

# Stop/status use the active run socket even if config was edited mid-run.
status_sock() {
    if [ -f "${LOCK_DIR}/status-sock" ]; then
        cat "${LOCK_DIR}/status-sock"
        return
    fi
    build_args || return
    printf '%s\n' "$STATUS_SOCKET"
}

# Print the live status payload from the running scrub's Unix-socket status
# server (key=value lines; empty if the server isn't up or curl is missing).
# Backs the Settings page's live-status panel.
status() {
    command -v curl >/dev/null 2>&1 || return 0
    curl -s --max-time 2 --unix-socket "$(status_sock)" "http://localhost/status" 2>/dev/null
}

case "${1:-running}" in
run) run ;;
running) running ;;
devices) devices ;;
fstypes) fstypes ;;
disk-id) disk_id_for "${2:-}" ;;
status) status ;;
stop) stop "${2:-}" ;;
*) echo "usage: $0 run|running|devices|fstypes|disk-id|status|stop" ;;
esac
