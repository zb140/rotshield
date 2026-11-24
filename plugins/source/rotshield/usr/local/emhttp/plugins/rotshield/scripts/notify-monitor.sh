#!/bin/bash
# Minute cron task, including manual scans; no resident monitor process.
set -u
# Bound the entire cron tick, including parsing, not just curl/mail. Missing
# timeout means skip notifications rather than risk an unbounded child.
if [ "${1:-}" != --bounded ]; then
    command -v timeout >/dev/null 2>&1 || exit 0
    timeout -k 1s 10s bash "$0" --bounded >/dev/null 2>&1 || :
    exit 0
fi
PLUGIN_DIR="${ROTSHIELD_PLUGIN_DIR:-${PLUGIN_DIR:-/usr/local/emhttp/plugins/rotshield}}"
CONFIG_DIR="${ROTSHIELD_CONFIG_DIR:-/boot/config/plugins/rotshield}"
RUNS_DIR="${CONFIG_DIR}/runs"
LOCK_DIR="${RUNS_DIR}/.lock"
[ -d "$LOCK_DIR" ] || exit 0
. "${PLUGIN_DIR}/scripts/notify-common.sh"
# Never queue cron ticks. Keep the same inode across runs; revalidate identity
# after taking the lock so a previous tick cannot notify for a replacement run.
exec 9>"${RUNS_DIR}/.notify.lock" || exit 0
flock -n 9 || exit 0
active_run() {
    [ -f "$LOCK_DIR/notify-ready" ] &&
        [ ! -e "$LOCK_DIR/notify-done" ] &&
        [ ! -e "$LOCK_DIR/stop-pending" ] &&
        [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" = "$pid" ] &&
        kill -0 "$pid" 2>/dev/null &&
        ! grep -q 'finished:' "$run_log"
}
pid="$(cat "$LOCK_DIR/pid" 2>/dev/null)"
run_log="$(cat "$LOCK_DIR/run-log" 2>/dev/null)"
start="$(cat "$LOCK_DIR/start-epoch" 2>/dev/null)"
total="$(cat "$LOCK_DIR/total" 2>/dev/null)"
sock="$(cat "$LOCK_DIR/status-sock" 2>/dev/null)"
[[ "$pid" =~ ^[0-9]+$ && "$start" =~ ^[0-9]+$ && "$total" =~ ^[0-9]+$ ]] || exit 0
[ -f "$run_log" ] && active_run || exit 0
active="$(awk '/^\[[^]]+\] \([0-9]+\/[0-9]+\) scrubbing \// {dev=$NF} END {print dev}' "$run_log")"
payload="$(curl -fsS --max-time 2 --unix-socket "$sock" http://localhost/status 2>/dev/null)"
device="$(printf '%s\n' "$payload" | sed -n 's/^device=//p')"
state="$(printf '%s\n' "$payload" | sed -n 's/^state=//p')"
# A completed device may leave its socket alive briefly. Only use snapshots
# matching the current runner section and never double-count a final block.
if [ -z "$active" ] || [ "$device" != "$active" ] || [ -z "$state" ]; then
    payload=""
fi
if [ -n "$payload" ]; then
    message=""
    severity=warning
    # Hash the full path, not basename (mapper paths can share basenames).
    disk_key="$(printf '%s' "$device" | sha256sum)"
    disk_key="${disk_key%% *}"
    for key in sectors_mismatch sectors_read_error metadata_header_errors metadata_mirror_mismatches metadata_read_errors failed readback_failed not_frozen thaw_failed; do
        count="$(printf '%s\n' "$payload" | awk -F= -v k="$key" '$1==k {print $2; exit}')"
        [[ "$count" =~ ^[0-9]+$ ]] || continue
        # Normalize before shell arithmetic, including leading zeroes.
        level="$(awk -v n="$count" 'BEGIN {print n>=100?2:(n>0?1:0)}')"
        [ "$level" -gt 0 ] || continue
        case "$key" in failed | readback_failed | not_frozen | thaw_failed) level=2 ;; esac
        flag="$LOCK_DIR/notified-${disk_key}-${key}"
        previous="$(cat "$flag" 2>/dev/null)"
        [[ "$previous" =~ ^[12]$ ]] || previous=0
        [ "$level" -gt "$previous" ] || continue
        active_run || exit 0
        # Claim before attempting delivery; failure must not flood the next tick.
        printf '%s\n' "$level" >"$flag" || continue
        message="${message}${key}=${count}; "
        [ "$level" -eq 2 ] && severity=alert
    done
    if [ -n "$message" ] && active_run; then
        notify_scrub rotshield_issue 'RotShield problem detected' "$device: $message scan in progress (warning escalates at 100; recovery/write failures alert immediately)" "$severity"
    fi
fi
now="$(date +%s)"
stamp="$(date +%Y%m%d)"
[ "$((now - start))" -ge 21600 ] || exit 0
[ "$stamp" \> "$(date -d "@$start" +%Y%m%d)" ] || exit 0
[ "$(date +%H%M)" -ge 900 ] || exit 0
active_run || exit 0
[ ! -e "$LOCK_DIR/daily-$stamp" ] || exit 0
summary="$(job_summary "$run_log" "$total" "$start" "$payload")"
[ -n "$payload" ] || summary="$summary; live counters unavailable"
active_run || exit 0
: >"$LOCK_DIR/daily-$stamp" || exit 0
notify_scrub rotshield_daily 'RotShield daily progress' "$summary" normal
exit 0
