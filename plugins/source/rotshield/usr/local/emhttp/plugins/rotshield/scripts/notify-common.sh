#!/bin/bash
# Shared notification-only helpers. Never authorize writes or control scanners.
# Callers set CONFIG_DIR, RUNS_DIR and LOCK_DIR; config is never sourced from
# notification state. A persistent flock inode survives run-lock replacement.
notify_scrub() {
    local cmd="${ROTSHIELD_NOTIFY_CMD:-${NOTIFY_CMD:-/usr/local/emhttp/webGui/scripts/notify}}"
    [ -x "$cmd" ] && command -v timeout >/dev/null 2>&1 || return 0
    # Kill the command's process group too; never let mail delivery hold a run.
    timeout -k 1s 3s "$cmd" -e "$1" -s "$2" -d "$3" -i "$4" >/dev/null 2>&1 || :
    return 0
}

# Seal and drain an in-flight monitor before writing the terminal log marker.
# The monitor has a 10s lifetime, and each delivery has a 4s hard bound.
notify_quiesce() {
    : >"${LOCK_DIR}/notify-done" 2>/dev/null || :
    (flock -w 20 9 || :) 9>"${RUNS_DIR}/.notify.lock"
    return 0
}

# Latest complete snapshot per device, plus a validated live snapshot, without
# summing the same disk twice. Verified writes come ONLY from Rust's explicit
# post-readback record: recovered also includes dry runs; mounted is a boolean.
job_summary() {
    local file="$1" total="$2" start="$3" payload="${4:-}" now
    now="$(date +%s)"
    [[ "$start" =~ ^[0-9]+$ ]] || start="$now"
    {
        cat "$file" 2>/dev/null
        printf '\nstatus:\n%s\n' "$payload"
    } | awk -v total="$total" -v elapsed="$(((now - start) / 60))" '
    function flush( k,a) {
      # Rust emits progress_pct last; ignore snapshots cut off during output.
      # Final disk results beat live responses fetched before completion.
      if (dev != "" && complete && !terminal[dev]) {
        for (k in values) {
          split(k,a,SUBSEP)
          if (a[1] == dev) delete values[k]
        }
        for (k in block) values[dev SUBSEP k] = block[k]
      }
      delete block; dev=""; state=""; complete=0
    }
    /^status:$/ { flush(); inside=1; next }
    inside && /^progress_pct=[0-9]+([.][0-9]+)?$/ { complete=1; next }
    inside && /^device=/ { dev=substr($0,8); next }
    inside && /^state=/ { state=substr($0,7); next }
    inside && /^[a-z_]+=[0-9]+$/ {
      split($0,a,"="); block[a[1]]=a[2]; next
    }
    /^\[/ { flush(); inside=0 }
    /^\[[^]]+\] \([0-9]+\/[0-9]+\) scrubbing \// { active=$NF }
    /^\[[^]]+\] \([0-9]+\/[0-9]+\) \/[^ ]+: (OK|ISSUES FOUND|METADATA FATAL|ERROR|REFUSED)/ {
      disk=$0; sub(/^.*[)] /,"",disk); sub(/: .*/,"",disk); terminal[disk]=1
      if ($0 ~ /ERROR|REFUSED/) errors++
      else completed++
      active="none"
    }
    /RECOVERED via .*\(written \+ read-back verified\) dev=/ { verified++ }
    /RECOVERED via .*\(dry-run\) dev=/ { assessed++ }
    END {
      flush()
      for (i in values) { split(i,a,SUBSEP); sums[a[2]]+=values[i] }
      printf "%d/%d disks completed; active/last=%s; elapsed=%d min; runtime/refusal errors=%d; checksum mismatches=%d; read errors=%d; metadata errors=%d; recovered (includes assessment)=%d; dry-run recoverable=%d; verified repairs=%d; recovery failed=%d; skipped=%d; freeze unavailable=%d; readback failed=%d", completed,total,active==""?"unknown":active,elapsed,errors,sums["sectors_mismatch"],sums["sectors_read_error"],sums["metadata_header_errors"]+sums["metadata_mirror_mismatches"]+sums["metadata_read_errors"],sums["recovered"],assessed,verified,sums["failed"],sums["skipped"],sums["not_frozen"],sums["readback_failed"]
      if (sums["repaired_while_mounted"]>0) printf "; live-mount writes occurred: reboot/remount recommended (page-cache advisory)"

    }'
}

notify_job_started() (
    # Initialization precedes publication of ready; the monitor cannot overtake
    # the start attempt. Delivery is at-most-once, including delivery failures.
    flock -w 20 9 || exit 0
    origin=manual
    mode='read-only recovery assessment'
    [ "${ROTSHIELD_SCHEDULED:-0}" = 1 ] && origin=scheduled
    [ "${WRITE:-0}" = 1 ] && mode='repair requested'
    notify_scrub rotshield_job_started 'RotShield scan started' \
        "${origin} scan; $1 disks; mode=${mode}; started $2" normal
) 9>"${RUNS_DIR}/.notify.lock"

notify_job_finished() {
    local file="$1" kind="$2" rc="${3:-0}"
    # Seal BEFORE waiting; even a lock timeout must disable future monitors.
    : >"${LOCK_DIR}/notify-done" 2>/dev/null || :
    (
        flock -w 20 9 || exit 0
        [ ! -e "${LOCK_DIR}/notify-final-sent" ] || exit 0
        : >"${LOCK_DIR}/notify-final-sent" || exit 0
        local total start summary severity=normal outcome event=rotshield_job_finished
        total="$(cat "${LOCK_DIR}/total" 2>/dev/null)"
        start="$(cat "${LOCK_DIR}/start-epoch" 2>/dev/null)"
        summary="$(job_summary "$file" "${total:-0}" "$start")"
        outcome="$(awk '/finished:/ {line=$0} END {print line}' "$file" 2>/dev/null)"
        [ "$rc" = 0 ] || severity=warning
        case "$outcome" in *ERROR* | *FATAL* | *UNRECOVERABLE*) severity=alert ;; esac
        case "$summary" in *'live-mount writes occurred'*) [ "$severity" != normal ] || severity=warning ;; esac
        if grep -qx 'job_aborted=1' "$file"; then
            summary="Partial results (repair job aborted); $summary"
            severity=alert
        fi
        if [ -f "${LOCK_DIR}/safety-reason" ]; then
            summary="SAFETY INTERLOCK: $(cat "${LOCK_DIR}/safety-reason"); $summary"
            severity=alert
        fi
        if [ "$kind" = cancelled ]; then
            severity=warning
            event=rotshield_job_stopped
            summary="Partial results (last available counters); $summary"
        fi
        notify_scrub "$event" "RotShield scan $kind" "$summary; $outcome" "$severity"
    ) 9>"${RUNS_DIR}/.notify.lock"
    return 0
}
