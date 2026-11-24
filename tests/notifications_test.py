#!/usr/bin/env python3
"""Temporary-only notification integration tests; run with python3 tests/notifications_test.py.

Requires the same Unix utilities as the plugin (bash, curl, flock, timeout,
procps, coreutils), but no Python dependencies, root, disks, or running plugin.
The scanner is a gated Unix HTTP server, never the real scrub-rs binary.
"""

import datetime
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
PLUGIN = REPO / "plugins/source/rotshield/usr/local/emhttp/plugins/rotshield"
RC = REPO / "plugins/source/rotshield/etc/rc.d/rc.rotshield"
COUNTERS = (
    "sectors_checked sectors_ok sectors_stale sectors_no_csum stale_csum_branches "
    "isolation_truncated bytes_checked not_corrupt deduped "
    "sectors_mismatch sectors_read_error metadata_header_errors "
    "metadata_mirror_mismatches metadata_read_errors recovered failed skipped "
    "not_frozen readback_failed repaired_while_mounted thaw_failed"
).split()


def snapshot(device, state="running", **counts):
    # Rust terminates a full status response with progress_pct, not a blank line.
    return (
        "\n".join(
            [f"state={state}", f"device={device}"]
            + [f"{key}={counts.get(key, 0)}" for key in COUNTERS]
            + ["recovery=1", "progress_total=100", "progress_done=50"]
            + ["progress_pct=100.00" if state == "done" else "progress_pct=50.00"]
        )
        + "\n"
    )


def atomic(path, text):
    path = Path(path)
    pending = path.with_suffix(".new")
    pending.write_text(text)
    pending.replace(path)


def mock_main(kind, args):
    root = Path(os.environ["MOCK_ROOT"])
    if kind == "date":
        clock = json.loads((root / "clock").read_text())
        if args == ["+%s"]:
            print(clock["epoch"])
        else:
            epoch = clock["epoch"]
            if args[:1] == ["-d"]:
                if "start_date" in clock and args[-1] == "+%Y%m%d":
                    print(clock["start_date"])
                    return 0
                epoch = int(args[1][1:])
            print(
                datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime(
                    args[-1][1:]
                )
            )
        return 0
    if kind == "notify":
        record = dict(zip(args[::2], args[1::2]))
        record["time"] = time.monotonic()
        # One O_APPEND write: concurrent invocations cannot interleave JSON lines.
        fd = os.open(
            root / "notifications", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600
        )
        os.write(fd, (json.dumps(record) + "\n").encode())
        os.close(fd)
        mode = (root / "notify-mode").read_text()
        if mode == "hang":
            time.sleep(60)
        return 1 if mode == "fail" else 0
    if kind == "fsfreeze":
        with (root / "fsfreeze.calls").open("a") as stream:
            stream.write(json.dumps(args) + "\n")
        mode = (root / "fsfreeze-mode").read_text()
        if mode == "hang":
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            time.sleep(60)
        return 0 if mode == "ok" else 99
    if kind == "findmnt":
        discovery_device = root / "discovery-device"
        gated_device = (
            discovery_device.read_text()
            if discovery_device.exists()
            else str(root / "disk2")
        )
        if args[-1] == gated_device and (root / "discovery-gated").exists():
            (root / "discovery.ready").touch()
            deadline = time.monotonic() + 30
            while not (root / "discovery.release").exists():
                if time.monotonic() > deadline:
                    return 1
                time.sleep(0.02)
        print(root / "mount")
        return 0
    if kind == "scanner":
        device = args[0]
        name = Path(device).name
        (root / (name + ".args")).write_text(json.dumps(args))
        sock = None
        for index, arg in enumerate(args):
            if arg == "--status-sock":
                sock = args[index + 1]
            elif arg.startswith("--status-sock="):
                sock = arg.split("=", 1)[1]
        if sock is None:
            raise RuntimeError("scanner launched without a status socket")
        # Invalid-option tests may accidentally launch the scanner. Record that
        # regression without touching a real/default socket outside the fixture.
        if Path(sock).parent != root:
            return 2
        term_mode = root / (name + ".term-mode")
        if term_mode.exists():
            mode = term_mode.read_text()
            if mode == "ignore":
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
            else:

                def terminate(signum, frame):
                    (root / (name + ".term-received")).touch()
                    # Exercise FIFO draining after the pre-TERM status capture.
                    time.sleep(0.25)
                    if mode == "thaw-failed":
                        print("thaw_failed=1", flush=True)
                    elif mode == "clean":
                        print("repair_cleanup=complete", flush=True)
                    raise SystemExit(0)

                signal.signal(signal.SIGTERM, terminate)
        socket_path = Path(sock)
        socket_path.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(sock)
            server.listen(8)
            server.settimeout(0.05)
            (root / (name + ".ready")).touch()
            deadline = time.monotonic() + 40
            while not (root / (name + ".complete")).exists():
                if time.monotonic() > deadline:
                    raise RuntimeError("test never released scanner gate")
                try:
                    conn, _ = server.accept()
                except socket.timeout:
                    continue
                with conn:
                    conn.settimeout(1)
                    conn.recv(8192)
                    body = (root / (name + ".payload")).read_bytes()
                    try:
                        conn.sendall(
                            b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: "
                            + str(len(body)).encode()
                            + b"\r\n\r\n"
                            + body
                        )
                    except BrokenPipeError:
                        pass
            if not (root / (name + ".abort")).exists():
                print("scrub complete:", flush=True)
            records = root / (name + ".records")
            if records.exists():
                print(records.read_text(), end="")
            print("status:")
            print((root / (name + ".final")).read_text(), end="", flush=True)
        socket_path.unlink(missing_ok=True)
        return int((root / (name + ".rc")).read_text())
    raise ValueError(kind)


@unittest.skipUnless(
    os.name == "posix" and hasattr(socket, "AF_UNIX"), "Unix scripts required"
)
class NotificationLifecycle(unittest.TestCase):
    def setUp(self):
        for tool in ("bash", "curl", "flock", "timeout", "pgrep", "awk", "sha256sum"):
            if not shutil.which(tool):
                self.skipTest(f"required plugin utility missing: {tool}")
        self.tmp = tempfile.TemporaryDirectory(prefix="rs-notify-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.processes = []
        self.addCleanup(self.cleanup_processes)
        self.plugin = self.root / "plugin"
        self.config = self.root / "config"
        self.bin = self.root / "bin"
        self.cron = self.root / "cron"
        for path in (
            self.plugin / "scripts",
            self.plugin / "bin",
            self.config,
            self.bin,
            self.cron,
        ):
            path.mkdir(parents=True)
        for name in ("scrub.sh", "notify-common.sh", "notify-monitor.sh"):
            shutil.copy2(PLUGIN / "scripts" / name, self.plugin / "scripts" / name)
        shutil.copy2(RC, self.root / "rc.rotshield")
        shutil.copy2(__file__, self.root / "mock.py")
        self.env = os.environ.copy()
        # Do not inherit user/plugin overrides into a temporary test job.
        for key in list(self.env):
            if key.startswith("ROTSHIELD_") or key in (
                "BASH_ENV",
                "ENV",
                "PROC_NMDSTAT",
            ):
                self.env.pop(key)
        self.env.update(
            MOCK_ROOT=str(self.root),
            ROTSHIELD_PLUGIN_DIR=str(self.plugin),
            ROTSHIELD_CONFIG_DIR=str(self.config),
            ROTSHIELD_LOG=str(self.root / "plugin.log"),
            ROTSHIELD_NOTIFY_CMD=str(self.bin / "notify"),
            ROTSHIELD_CRON_DIR=str(self.cron),
            PROC_NMDSTAT=str(self.root / "nmdstat"),
            PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
            TZ="UTC",
            LC_ALL="C",
        )
        for path, kind in (
            (self.bin / "date", "date"),
            (self.bin / "notify", "notify"),
            (self.bin / "fsfreeze", "fsfreeze"),
            (self.bin / "findmnt", "findmnt"),
            (self.plugin / "bin/scrub-rs", "scanner"),
        ):
            path.write_text(
                "#!/bin/sh\nexec "
                + shlex.quote(sys.executable)
                + " "
                + shlex.quote(str(self.root / "mock.py"))
                + " --mock "
                + kind
                + ' "$@"\n'
            )
            path.chmod(0o755)
        # Never inspect real mount/device topology, even for mock repair runs.
        for name, body in (("lsblk", "exit 0"),):
            path = self.bin / name
            path.write_text("#!/bin/sh\n" + body + "\n")
            path.chmod(0o755)
        self.devices = [str(self.root / "disk1"), str(self.root / "disk2")]
        (self.root / "nmdstat").write_text(
            "".join(
                f"rdevName.{i}={dev}\nrdevOffset.{i}=64\n"
                for i, dev in enumerate(self.devices, 1)
            )
        )
        (self.root / "notify-mode").write_text("ok")
        (self.root / "fsfreeze-mode").write_text("fail")
        self.clock("2026-01-01T00:00:00")
        self.configure()
        for dev in self.devices:
            self.payload(dev)
            name = Path(dev).name
            (self.root / (name + ".final")).write_text(snapshot(dev, "done"))
            (self.root / (name + ".rc")).write_text("0")

    def cleanup_processes(self):
        # Each invocation has its own session; kill descendants even if bash exited.
        for proc in reversed(self.processes):
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait(timeout=5)

    def configure(self, write=0, extra=None):
        if extra is None:
            extra = f"--status-sock {self.root}/status.sock"
        self.cfg = self.config / "config.cfg"
        self.cfg.write_text(
            f"DEVICES={shlex.quote(' '.join(self.devices))}\n"
            f"WRITE={shlex.quote(str(write))}\n"
            f"EXTRA_OPTIONS={shlex.quote(extra)}\n"
            'SCHEDULE=custom\nCRON="17 4 * * 2"\n'
        )

    def clock(self, iso):
        epoch = int(
            datetime.datetime.fromisoformat(iso)
            .replace(tzinfo=datetime.timezone.utc)
            .timestamp()
        )
        atomic(self.root / "clock", json.dumps({"epoch": epoch}))

    def spawn(self, argv):
        # File output avoids pipe backpressure and preserves timeout diagnostics.
        output = self.root / f"process-{len(self.processes)}.out"
        with output.open("wb") as stream:
            proc = subprocess.Popen(
                argv,
                env=self.env,
                stdout=stream,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        proc.output_path = output
        self.processes.append(proc)
        return proc

    def command(self, argv, timeout=15):
        proc = self.spawn(argv)
        try:
            rc = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.fail(f"Command timed out: {argv}\n{proc.output_path.read_text()}")
        self.assertEqual(rc, 0, proc.output_path.read_text())
        return proc.output_path.read_text()

    def scrub(self, *args):
        return ["bash", str(self.plugin / "scripts/scrub.sh"), *args]

    def wait_for(self, predicate, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.03)
        self.fail(
            "Gate timed out\n"
            + "\n".join(p.output_path.read_text() for p in self.processes)
            + "\n"
            + self.log_text()
        )

    def start(self):
        self.runner = self.spawn(self.scrub("run"))
        self.wait_for(lambda: (self.root / "disk1.ready").exists())
        self.assertIsNone(self.runner.poll())
        self.assertEqual(len(self.events("rotshield_job_started")), 1)

    def payload(self, dev, **counts):
        atomic(self.root / (Path(dev).name + ".payload"), snapshot(dev, **counts))

    def release(self, index):
        (self.root / f"disk{index}.complete").touch()

    def finish(self, expected_rc=0):
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.release(2)
        self.assertEqual(self.runner.wait(timeout=12), expected_rc, self.log_text())
        self.assertFalse((self.config / "runs/.lock").exists())
        self.assertEqual(len(self.events("rotshield_job_finished")), 1)

    def tick(self):
        self.command(["bash", str(self.plugin / "scripts/notify-monitor.sh")])

    def events(self, event=None):
        path = self.root / "notifications"
        rows = (
            [json.loads(line) for line in path.read_text().splitlines()]
            if path.exists()
            else []
        )
        return [row for row in rows if event is None or row["-e"] == event]

    def log_text(self):
        return "\n".join(
            p.read_text() for p in (self.config / "runs").glob("run-*.log")
        )

    def assert_quiet(self):
        before = self.events()
        self.tick()
        self.tick()
        self.assertEqual(self.events(), before, "monitor notified after terminal job")

    def test_sequential_job_duplicate_lock_and_completion(self):
        self.start()
        self.assertFalse((self.root / "disk2.ready").exists())
        self.assertIn("already running", self.command(self.scrub("run")))
        self.assertEqual(len(self.events("rotshield_job_started")), 1)
        self.tick()
        self.assertEqual(len(self.events()), 1)
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.clock("2026-01-02T09:00:00")
        self.tick()
        self.assertIn("1/2 disks completed", self.events("rotshield_daily")[0]["-d"])
        self.finish()
        final = self.events("rotshield_job_finished")[0]
        self.assertIn("2/2 disks completed", final["-d"])
        self.assertEqual(final["-i"], "normal")
        self.assertEqual(self.log_text().count("partition offset +64"), 2)
        for index in (1, 2):
            args = json.loads((self.root / f"disk{index}.args").read_text())
            self.assertNotIn("--repair", args)
            self.assertIn("+64", args)
        self.assert_quiet()

    def test_start_reports_manual_mode_and_time(self):
        self.env["ROTSHIELD_SCHEDULED"] = "0"
        self.start()
        text = self.events("rotshield_job_started")[0]["-d"]
        for part in (
            "manual scan",
            "2 disks",
            "read-only recovery assessment",
            "2026-01-01 00:00:00",
        ):
            self.assertIn(part, text)
        self.finish()

    def test_start_reports_scheduled_repair_request(self):
        self.configure(write=1)
        self.env["ROTSHIELD_SCHEDULED"] = "1"
        self.start()
        text = self.events("rotshield_job_started")[0]["-d"]
        self.assertIn("scheduled scan", text)
        self.assertIn("repair requested", text)
        self.finish()

    def test_aborted_disk_sends_one_problematic_job_summary(self):
        (self.root / "disk1.abort").touch()
        (self.root / "disk1.rc").write_text("1")
        self.start()
        self.finish(1)
        final = self.events("rotshield_job_finished")[0]
        self.assertIn("1/2 disks completed", final["-d"])
        self.assertIn("runtime/refusal errors=1", final["-d"])
        self.assertIn("finished: ERROR", final["-d"])
        self.assertEqual(final["-i"], "alert")
        self.assert_quiet()

    def test_empty_targets_do_not_notify(self):
        self.cfg.write_text(chr(10).join(('DEVICES=""', "WRITE=0", "")))
        proc = self.spawn(self.scrub("run"))
        self.assertEqual(proc.wait(timeout=5), 1)
        self.assertEqual(self.events(), [])
        self.assertFalse((self.config / "runs/.lock").exists())

    def test_immediate_dedup_threshold_and_recovery_escalation(self):
        self.start()
        self.payload(self.devices[0], sectors_mismatch=1)
        self.tick()
        self.tick()
        self.assertEqual(len(self.events("rotshield_issue")), 1)
        self.assertEqual(self.events("rotshield_issue")[-1]["-i"], "warning")
        self.payload(self.devices[0], sectors_mismatch=99)
        self.tick()
        self.assertEqual(len(self.events("rotshield_issue")), 1)
        self.payload(self.devices[0], sectors_mismatch=100)
        self.tick()
        self.assertEqual(len(self.events("rotshield_issue")), 2)
        self.assertEqual(self.events("rotshield_issue")[-1]["-i"], "alert")
        for key in ("failed", "readback_failed", "not_frozen"):
            self.payload(self.devices[0], sectors_mismatch=100, **{key: 1})
            self.tick()
            self.assertEqual(self.events("rotshield_issue")[-1]["-i"], "alert")
            self.assertIn(f"{key}=1", self.events("rotshield_issue")[-1]["-d"])
            count = len(self.events())
            self.tick()
            self.assertEqual(len(self.events()), count)
        self.assertEqual(len(self.events("rotshield_issue")), 5)
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.payload(self.devices[1], sectors_mismatch=1)
        self.tick()
        self.assertEqual(
            len(self.events("rotshield_issue")), 6, "dedup must be per disk"
        )
        self.finish()
        self.assert_quiet()

    def test_daily_date_six_hour_and_nine_am_boundaries(self):
        self.clock("2026-01-01T23:00:00")
        self.start()
        for instant in (
            "2026-01-01T23:59:59",
            "2026-01-02T04:59:59",
            "2026-01-02T08:59:59",
        ):
            self.clock(instant)
            self.tick()
            self.assertEqual(self.events("rotshield_daily"), [], instant)
        self.clock("2026-01-02T09:00:00")
        self.tick()
        self.tick()
        self.assertEqual(len(self.events("rotshield_daily")), 1)
        self.assertIn("0/2 disks completed", self.events("rotshield_daily")[0]["-d"])
        self.clock("2026-01-03T09:00:00")
        self.tick()
        self.assertEqual(len(self.events("rotshield_daily")), 2)
        self.finish()

    def test_daily_requires_new_date_and_full_six_hours(self):
        self.start()
        self.clock("2026-01-01T09:00:00")
        self.tick()
        self.assertEqual(self.events("rotshield_daily"), [], "same date is not daily")
        # In UTC, next-date + 9am already implies more than six hours.
        # Override only date -d's calendar answer to independently exercise the
        # elapsed-time guard, rather than accidentally testing the date guard twice.
        lock = self.config / "runs/.lock"
        self.clock("2026-01-02T09:00:00")
        clock = json.loads((self.root / "clock").read_text())
        epoch = clock["epoch"]
        clock["start_date"] = "20260101"
        atomic(self.root / "clock", json.dumps(clock))
        (lock / "start-epoch").write_text(str(epoch - 21599))
        self.tick()
        self.assertEqual(self.events("rotshield_daily"), [])
        (lock / "start-epoch").write_text(str(epoch - 21600))
        self.tick()
        self.assertEqual(len(self.events("rotshield_daily")), 1)
        self.finish()

    def test_read_only_recoverable_is_not_verified_repair(self):
        dev = self.devices[0]
        (self.root / "disk1.records").write_text(
            f"RECOVERED via P (dry-run) dev={dev}\n"
        )
        (self.root / "disk1.final").write_text(
            snapshot(dev, "done", recovered=1, sectors_mismatch=1)
        )
        (self.root / "disk1.rc").write_text("4")
        self.start()
        self.finish(4)
        text = self.events("rotshield_job_finished")[0]["-d"]
        self.assertIn("dry-run recoverable=1", text)
        self.assertIn("verified repairs=0", text)
        self.assertIn("recovered (includes assessment)=1", text)

    def test_verified_repair_requires_explicit_record(self):
        self.configure(write=1)
        dev = self.devices[0]
        (self.root / "disk1.records").write_text(
            f"RECOVERED via P (written + read-back verified) dev={dev}\n"
        )
        (self.root / "disk1.final").write_text(
            snapshot(dev, "done", recovered=3, repaired_while_mounted=1)
        )
        self.start()
        self.finish()
        final = self.events("rotshield_job_finished")[0]
        self.assertIn("verified repairs=1", final["-d"])
        self.assertIn("dry-run recoverable=0", final["-d"])
        self.assertIn("reboot/remount", final["-d"])
        self.assertEqual(final["-i"], "warning")
        self.assertIn("--repair", json.loads((self.root / "disk1.args").read_text()))
        self.assertIn(
            "--freeze-mount", json.loads((self.root / "disk1.args").read_text())
        )

    def test_cancel_retains_partial_counters_releases_lock(self):
        self.start()
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.payload(self.devices[1], sectors_mismatch=7, recovered=2)
        self.command(self.scrub("stop"), timeout=18)
        self.runner.wait(timeout=3)
        self.assertFalse((self.config / "runs/.lock").exists())
        stopped = self.events("rotshield_job_stopped")
        self.assertEqual(len(stopped), 1)
        for part in (
            "Partial results",
            "1/2 disks completed",
            "checksum mismatches=7",
            "verified repairs=0",
        ):
            self.assertIn(part, stopped[0]["-d"])
        self.assertEqual(self.events("rotshield_job_finished"), [])
        self.assertIn("state=cancelled", self.log_text())
        self.assertIn("finished: CANCELLED", self.log_text())
        self.assertEqual(self.command(self.scrub("running")).strip(), "0")
        self.assert_quiet()

    def test_notification_failures_do_not_stop_job_or_retry(self):
        (self.root / "notify-mode").write_text("fail")
        self.start()
        self.payload(self.devices[0], sectors_read_error=1)
        self.tick()
        self.tick()
        self.assertEqual(len(self.events("rotshield_issue")), 1)
        self.finish()
        self.assert_quiet()

    def test_notification_hangs_are_bounded_and_job_completes(self):
        (self.root / "notify-mode").write_text("hang")
        start = time.monotonic()
        self.start()
        self.payload(self.devices[0], sectors_mismatch=1)
        self.tick()
        self.tick()
        self.assertEqual(len(self.events("rotshield_issue")), 1)
        self.finish()
        self.assertLess(time.monotonic() - start, 20)
        self.assert_quiet()

    def test_terminal_window_blocks_monitor_and_late_stop(self):
        self.start()
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.payload(self.devices[1], sectors_mismatch=100)
        self.clock("2026-01-02T09:00:00")
        # Keep final delivery in flight after the terminal log marker, before
        # EXIT removes the run lock. This is distinct from an absent-lock tick.
        (self.root / "notify-mode").write_text("hang")
        self.release(2)
        self.wait_for(lambda: bool(self.events("rotshield_job_finished")))
        self.assertTrue((self.config / "runs/.lock").exists())
        self.assert_quiet()
        self.assertIn("already finished", self.command(self.scrub("stop")))
        self.assertEqual(self.runner.wait(timeout=8), 0)
        self.assertNotIn("CANCELLED", self.log_text())
        self.assertFalse((self.config / "runs/.lock").exists())
        self.assertEqual(self.events("rotshield_job_stopped"), [])

    def assert_refused_before_launch(self):
        # Each subcase needs fresh evidence, even when the preceding refusal
        # regressed and launched a complete job or retained a safety lock.
        for pattern in ("disk*.args", "disk*.ready", "notifications"):
            for path in self.root.glob(pattern):
                path.unlink()
        shutil.rmtree(self.config / "runs", ignore_errors=True)
        # Open gates so a regression that launches the scanner fails promptly.
        self.release(1)
        self.release(2)
        proc = self.spawn(self.scrub("run"))
        rc = proc.wait(timeout=8)
        self.assertFalse(
            list(self.root.glob("disk*.args")), proc.output_path.read_text()
        )
        self.assertNotEqual(rc, 0, proc.output_path.read_text())
        self.assertEqual(self.events("rotshield_job_started"), [])
        self.assertFalse((self.config / "runs/.lock").exists())

    def test_write_rejects_every_value_except_zero_or_one(self):
        for value in ("", "2", "-1", "01", "true", "false", "1 0", "1;true"):
            with self.subTest(write=value):
                self.configure(write=value)
                self.assert_refused_before_launch()

    def test_extra_options_reject_policy_overrides_operands_and_unknowns(self):
        options = (
            "--repair",
            "--repair=1",
            "--no-freeze",
            "--no-freeze=0",
            "--freeze-mount /tmp/mount",
            "--freeze-mount=/tmp/mount",
            "--offset 64",
            "--offset=64",
            "disk3",
            "--unknown",
            "--",
            "--freeze-timeout 1",
            "--meta-chunk 1",
            "--max-read-blocks 1",
            "--batch-max 2 --repair",
            "--status-sock /tmp/s --no-freeze",
        )
        for extra in options:
            with self.subTest(extra=extra):
                self.configure(extra=extra)
                self.assert_refused_before_launch()

    def test_extra_options_reject_missing_invalid_and_duplicate_values(self):
        options = ["--status-sock", "--status-sock=", "--status-sock --repair"]
        for flag, invalid in (
            ("--batch-max", ("0", "-1", "1.5", "nan", "1e2")),
            ("--batch-idle", ("-1", "nan", "inf", "Infinity", "1e999")),
            ("--freeze-max", ("0", "-1", "nan", "inf", "Infinity", "1e999")),
        ):
            options.append(flag)
            options.append(flag + "=")
            for value in invalid:
                options.extend((f"{flag} {value}", f"{flag}={value}"))
        for flag, value in (
            ("--status-sock", str(self.root / "status.sock")),
            ("--batch-max", "2"),
            ("--batch-idle", "0.5"),
            ("--freeze-max", "1.5"),
        ):
            options.extend(
                (
                    f"{flag} {value} {flag} {value}",
                    f"{flag}={value} {flag}={value}",
                    f"{flag} {value} {flag}={value}",
                )
            )
        for extra in options:
            with self.subTest(extra=extra):
                self.configure(extra=extra)
                self.assert_refused_before_launch()

    def assert_tuning_arguments(self, equals=False):
        pairs = (
            ("--status-sock", str(self.root / "status.sock")),
            ("--batch-max", "7"),
            ("--batch-idle", "0"),
            ("--freeze-max", "0.25"),
        )
        self.configure(
            extra=" ".join(
                f"{flag}={value}" if equals else f"{flag} {value}"
                for flag, value in pairs
            )
        )
        self.start()
        self.finish()
        args = json.loads((self.root / "disk1.args").read_text())
        # The plugin may normalize = syntax for the CLI, which only accepts
        # separate flag/value arguments.
        for flag, value in pairs:
            self.assertEqual(args.count(flag), 1, args)
            self.assertEqual(args[args.index(flag) + 1], value)
        self.assertNotIn("--repair", args)

    def test_supported_tuning_options(self):
        self.assert_tuning_arguments()

    def test_supported_tuning_options_equals_syntax(self):
        self.assert_tuning_arguments(equals=True)

    def test_generated_mount_argument_preserves_spaces_and_metacharacters(self):
        mount = str(self.root / "mount with spaces;[ab]*$(touch SHOULD_NOT_EXIST)")
        (self.bin / "findmnt").write_text(
            "#!/bin/sh\nprintf '%s\\n' " + shlex.quote(mount) + "\n"
        )
        self.configure(write=1)
        self.start()
        self.finish()
        args = json.loads((self.root / "disk1.args").read_text())
        self.assertEqual(args[args.index("--freeze-mount") + 1], mount)
        self.assertEqual(args.count("--repair"), 1)

    def test_extra_options_do_not_expand_globs_or_interpret_shell_quotes(self):
        sock = str(self.root / "status[ab]*;literal.sock")
        (self.root / "statusa-expanded;literal.sock").touch()
        self.configure(extra=f'--status-sock {sock} --batch-max "7"')
        # Quotes are literal under read -ra, not shell syntax: "7" is not an int.
        self.assert_refused_before_launch()

    def test_status_socket_metacharacters_are_literal(self):
        sock = str(self.root / "status[ab]*;literal.sock")
        (self.root / "statusa-expanded;literal.sock").touch()
        self.configure(extra=f"--status-sock {sock}")
        self.start()
        self.finish()
        args = json.loads((self.root / "disk1.args").read_text())
        self.assertEqual(args[args.index("--status-sock") + 1], sock)

    def repair_terminal(self, rc, *, missing_complete=False, records=""):
        self.configure(write=1)
        (self.root / "disk1.rc").write_text(str(rc))
        (self.root / "disk1.records").write_text(records)
        if missing_complete:
            (self.root / "disk1.abort").touch()
        self.start()
        self.release(2)  # A continuation regression must not hang the test.
        self.release(1)
        self.assertNotEqual(self.runner.wait(timeout=8), 0, self.log_text())
        self.assertFalse((self.root / "disk2.args").exists(), self.log_text())
        final = self.events("rotshield_job_finished")
        self.assertEqual(len(final), 1)
        self.assertEqual(final[0]["-i"], "alert")
        self.assertIn("verified repairs=0", final[0]["-d"])
        self.assertIn("partial", final[0]["-d"].lower())
        self.assertNotIn("2/2 disks completed", final[0]["-d"])

    def assert_safety_lock(self):
        lock = self.config / "runs/.lock"
        self.assertTrue(lock.is_dir())
        self.assertTrue(
            any(
                (lock / name).exists()
                for name in (
                    "stop-pending",
                    "safety-pending",
                )
            ),
            list(lock.iterdir()),
        )
        self.assertEqual(self.command(self.scrub("running")).strip(), "1")
        before = self.events()
        proc = self.spawn(self.scrub("run"))
        proc.wait(timeout=5)
        self.assertEqual(self.events(), before)
        self.assertFalse((self.root / "disk2.args").exists())
        self.assertTrue(lock.is_dir())

    def test_repair_runtime_error_stops_next_disk(self):
        self.repair_terminal(1)

    def test_repair_runtime_error_with_confirmed_cleanup_does_not_retain_lock(self):
        self.repair_terminal(1, missing_complete=True, records="repair_cleanup=complete" + chr(10))
        self.assertFalse((self.config / "runs/.lock").exists())

    def test_live_thaw_failure_alert_is_immediate_and_deduplicated(self):
        self.start()
        self.payload(self.devices[0], thaw_failed=1)
        self.tick()
        self.tick()
        issue = self.events("rotshield_issue")
        self.assertEqual(len(issue), 1)
        self.assertEqual(issue[0]["-i"], "alert")
        self.assertIn("thaw_failed=1", issue[0]["-d"])
        self.finish()

    def test_repair_usage_error_stops_next_disk(self):
        self.repair_terminal(2)

    def test_repair_metadata_fatal_stops_next_disk(self):
        self.repair_terminal(6)

    def test_repair_unexpected_exit_stops_next_disk(self):
        self.repair_terminal(42)

    def test_repair_missing_completion_even_with_zero_exit_retains_lock(self):
        self.repair_terminal(0, missing_complete=True)
        self.assert_safety_lock()

    def test_repair_early_runtime_without_completion_retains_lock(self):
        self.repair_terminal(1, missing_complete=True)
        self.assert_safety_lock()

    def test_exact_thaw_failure_record_retains_lock(self):
        # Even an otherwise clean exit must not override explicit thaw failure.
        self.repair_terminal(0, records="thaw_failed=1\n")
        self.assert_safety_lock()
        text = self.events("rotshield_job_finished")[0]["-d"].lower()
        self.assertTrue("thaw" in text or "frozen" in text, text)

    def test_thaw_failure_lookalike_is_not_a_safety_marker(self):
        self.configure(write=1)
        (self.root / "disk1.records").write_text(
            "thaw_failed=0\nnot_thaw_failed=1\nthaw_failed=10\n"
        )
        self.start()
        self.finish()

    def test_repair_findings_continue_to_next_disk(self):
        # All three data-result codes are ordinary findings, not runtime failure.
        (self.root / "disk1.rc").write_text("3")
        (self.root / "disk2.rc").write_text("5")
        self.configure(write=1)
        self.start()
        self.finish(5)

    def test_repair_unrecoverable_findings_continue_to_next_disk(self):
        (self.root / "disk1.rc").write_text("5")
        self.configure(write=1)
        self.start()
        self.finish(5)

    def test_repair_recoverable_findings_continue_to_next_disk(self):
        (self.root / "disk1.rc").write_text("4")
        self.configure(write=1)
        self.start()
        self.finish(4)

    def start_stoppable(self, mode, write=1):
        self.configure(write=write)
        (self.root / "disk1.term-mode").write_text(mode)
        self.payload(self.devices[0], sectors_mismatch=7)
        self.start()

    def fsfreeze_calls(self):
        path = self.root / "fsfreeze.calls"
        return (
            [json.loads(line) for line in path.read_text().splitlines()]
            if path.exists()
            else []
        )

    def stop_and_assert(self, *, aborted=False, forced=False, max_seconds=8):
        started = time.monotonic()
        proc = self.spawn(self.scrub("stop"))
        rc = proc.wait(timeout=max_seconds + 2)
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, max_seconds, proc.output_path.read_text())
        self.assertEqual(rc, 1 if aborted else 0, proc.output_path.read_text())
        self.runner.wait(timeout=3)
        self.assertFalse((self.root / "disk2.args").exists(), self.log_text())
        if forced:
            self.assertIn("escalating to SIGKILL", proc.output_path.read_text())
        else:
            self.assertNotIn("escalating to SIGKILL", proc.output_path.read_text())
        if aborted:
            self.assertTrue((self.config / "runs/.lock/stop-pending").exists())
            self.assert_safety_lock()
            self.assertIn("job_aborted=1", self.log_text())
            self.assertIn("finished: SAFETY_ABORT", self.log_text())
            self.assertNotIn("finished: CANCELLED", self.log_text())
            self.assertEqual(self.events("rotshield_job_stopped"), [])
            final = self.events("rotshield_job_finished")
            self.assertEqual(len(final), 1)
            self.assertEqual(final[0]["-i"], "alert")
            self.assertIn("partial", final[0]["-d"].lower())
            self.assertIn("verified repairs=0", final[0]["-d"])
        else:
            self.assertFalse((self.config / "runs/.lock").exists())
            self.assertEqual(self.command(self.scrub("running")).strip(), "0")
            self.assertIn("finished: CANCELLED", self.log_text())
            self.assertEqual(self.events("rotshield_job_finished"), [])
            stopped = self.events("rotshield_job_stopped")
            self.assertEqual(len(stopped), 1)
            self.assertIn("checksum mismatches=7", stopped[0]["-d"])
        self.assert_quiet()

    def test_repair_stop_drains_delayed_thaw_failure_record(self):
        self.start_stoppable("thaw-failed")
        self.stop_and_assert(aborted=True)
        self.assertTrue((self.root / "disk1.term-received").exists())
        self.assertIn("\nthaw_failed=1\n", self.log_text())
        self.assertEqual(self.fsfreeze_calls(), [])

    def test_repair_stop_clean_cleanup_cancels(self):
        self.start_stoppable("clean")
        self.stop_and_assert()
        self.assertIn("\nrepair_cleanup=complete\n", self.log_text())
        self.assertEqual(self.fsfreeze_calls(), [])

    def test_repair_stop_without_cleanup_record_retains_lock(self):
        self.start_stoppable("no-cleanup")
        self.stop_and_assert(aborted=True)
        self.assertTrue((self.root / "disk1.term-received").exists())
        self.assertNotIn("repair_cleanup=complete", self.log_text())
        self.assertEqual(self.fsfreeze_calls(), [])

    def test_repair_stop_uses_recorded_mode_and_socket_after_config_edit(self):
        self.start_stoppable("no-cleanup")
        self.configure(write=0, extra=f"--status-sock {self.root}/different.sock")
        # Both status and Stop must use the live run's socket, not new config.
        self.assertIn("sectors_mismatch=7", self.command(self.scrub("status")))
        self.stop_and_assert(aborted=True)
        self.assertEqual(self.fsfreeze_calls(), [])

    def test_forced_repair_stop_successful_emergency_thaw_cancels(self):
        self.start_stoppable("ignore")
        (self.root / "fsfreeze-mode").write_text("ok")
        # A mid-run edit must not disable the repair run's emergency thaw.
        self.configure(write=0, extra=f"--status-sock {self.root}/different.sock")
        self.stop_and_assert(forced=True, max_seconds=18)
        self.assertEqual(self.fsfreeze_calls(), [["-u", str(self.root / "mount")]])

    def test_forced_repair_stop_failed_emergency_thaw_retains_lock(self):
        self.start_stoppable("ignore")
        self.stop_and_assert(aborted=True, forced=True, max_seconds=18)
        self.assertEqual(self.fsfreeze_calls(), [["-u", str(self.root / "mount")]])

    def test_forced_repair_stop_hung_emergency_thaw_is_bounded(self):
        self.start_stoppable("ignore")
        (self.root / "fsfreeze-mode").write_text("hang")
        self.stop_and_assert(aborted=True, forced=True, max_seconds=18)
        self.assertEqual(self.fsfreeze_calls(), [["-u", str(self.root / "mount")]])

    def test_forced_read_only_stop_never_thaws_after_config_edit(self):
        self.start_stoppable("ignore", write=0)
        self.configure(write=1, extra=f"--status-sock {self.root}/different.sock")
        self.stop_and_assert(forced=True, max_seconds=18)
        self.assertEqual(self.fsfreeze_calls(), [])

    def test_stop_during_disk_discovery_prevents_next_scanner_launch(self):
        (self.root / "discovery-gated").touch()
        self.start()
        self.payload(self.devices[0], sectors_mismatch=7)
        (self.root / "disk1.final").write_text(
            snapshot(self.devices[0], "done", sectors_mismatch=7)
        )
        self.release(1)
        self.wait_for(lambda: (self.root / "discovery.ready").exists())
        stop = self.spawn(self.scrub("stop"))
        self.wait_for(lambda: (self.config / "runs/.lock/stop-pending").exists())
        self.assertFalse((self.root / "disk2.args").exists())
        (self.root / "discovery.release").touch()
        self.assertEqual(stop.wait(timeout=8), 0, stop.output_path.read_text())
        self.runner.wait(timeout=3)
        self.assertFalse((self.root / "disk2.args").exists(), self.log_text())
        self.assertFalse((self.config / "runs/.lock").exists())
        self.assertEqual(len(self.events("rotshield_job_stopped")), 1)
        self.assertEqual(self.events("rotshield_job_finished"), [])

    def test_previous_disk_cleanup_cannot_certify_current_repair_stop(self):
        self.configure(write=1)
        (self.root / "disk1.records").write_text("repair_cleanup=complete\n")
        (self.root / "disk2.term-mode").write_text("no-cleanup")
        self.start()
        self.release(1)
        self.wait_for(lambda: (self.root / "disk2.ready").exists())
        self.assertIn("\nrepair_cleanup=complete\n", self.log_text())
        stop = self.spawn(self.scrub("stop"))
        self.assertEqual(stop.wait(timeout=8), 1, stop.output_path.read_text())
        self.runner.wait(timeout=3)
        self.assertTrue((self.root / "disk2.term-received").exists())
        log = self.log_text()
        self.assertEqual(log.count("repair_cleanup=complete"), 1, log)
        self.assertIn("finished: SAFETY_ABORT", log)
        self.assertNotIn("finished: CANCELLED", log)
        self.assertTrue((self.config / "runs/.lock/stop-pending").exists())
        self.assertEqual(self.command(self.scrub("running")).strip(), "1")
        final = self.events("rotshield_job_finished")
        self.assertEqual(len(final), 1)
        self.assertEqual(final[0]["-i"], "alert")
        self.assertEqual(self.events("rotshield_job_stopped"), [])
        self.assertEqual(self.fsfreeze_calls(), [])
        before = self.events()
        retry = self.spawn(self.scrub("run"))
        retry.wait(timeout=5)
        self.assertEqual(self.events(), before)
        self.assertTrue((self.config / "runs/.lock/stop-pending").exists())
        self.assert_quiet()

    def assert_blocked_discovery_forced_stop_is_safe(self):
        self.wait_for(lambda: (self.root / "discovery.ready").exists())
        self.assertFalse((self.root / "discovery.release").exists())
        started = time.monotonic()
        stop = self.spawn(self.scrub("stop"))
        self.assertEqual(stop.wait(timeout=20), 0, stop.output_path.read_text())
        self.assertLess(time.monotonic() - started, 18, stop.output_path.read_text())
        self.assertIn("escalating to SIGKILL", stop.output_path.read_text())
        self.runner.wait(timeout=3)
        self.assertEqual(self.fsfreeze_calls(), [])
        self.assertFalse((self.config / "runs/.lock").exists())
        self.assertEqual(self.command(self.scrub("running")).strip(), "0")
        self.assertIn("finished: CANCELLED", self.log_text())
        self.assertNotIn("finished: SAFETY_ABORT", self.log_text())
        self.assertEqual(len(self.events("rotshield_job_stopped")), 1)
        self.assertEqual(self.events("rotshield_job_finished"), [])
        self.assert_quiet()

    def test_forced_stop_before_first_scanner_skips_emergency_thaw(self):
        self.configure(write=1)
        (self.root / "discovery-device").write_text(self.devices[0])
        (self.root / "discovery-gated").touch()
        self.runner = self.spawn(self.scrub("run"))
        self.assert_blocked_discovery_forced_stop_is_safe()
        self.assertEqual(list(self.root.glob("disk*.args")), [])

    def test_forced_stop_between_clean_disks_skips_emergency_thaw(self):
        self.configure(write=1)
        (self.root / "disk1.records").write_text("repair_cleanup=complete\n")
        (self.root / "discovery-gated").touch()
        self.start()
        self.release(1)
        self.assert_blocked_discovery_forced_stop_is_safe()
        self.assertTrue((self.root / "disk1.args").exists())
        self.assertFalse((self.root / "disk2.args").exists())
        self.assertIn("\nrepair_cleanup=complete\n", self.log_text())

    def test_custom_cron_preserves_write_zero(self):
        original = self.cfg.read_bytes()
        self.command(["bash", str(self.root / "rc.rotshield"), "start"])
        cron = (self.cron / "rotshield").read_text()
        self.assertTrue(cron.startswith("17 4 * * 2 root "), cron)
        self.assertIn("ROTSHIELD_SCHEDULED=1", cron)
        self.assertIn(f"{self.plugin}/scripts/scrub.sh run", cron)
        self.assertNotIn("--repair", cron)
        self.assertEqual(self.cfg.read_bytes(), original)
        self.assertEqual(
            (self.cron / "rotshield-monitor").read_text(),
            f"* * * * * root {self.plugin}/scripts/notify-monitor.sh\n",
        )
        self.command(["bash", str(self.root / "rc.rotshield"), "restart"])
        self.assertEqual((self.cron / "rotshield").read_text(), cron)
        self.assertEqual(self.cfg.read_bytes(), original)
        self.command(["bash", str(self.root / "rc.rotshield"), "stop"])
        self.assertEqual(list(self.cron.iterdir()), [])

    def summary(self, log, payload):
        file = self.root / "summary.log"
        file.write_text(log)
        return self.command(
            [
                "bash",
                "-c",
                '. "$1"; job_summary "$2" 2 "$3" "$4"',
                "summary",
                str(self.plugin / "scripts/notify-common.sh"),
                str(file),
                str(json.loads((self.root / "clock").read_text())["epoch"]),
                payload,
            ]
        )

    def test_summary_rejects_truncated_snapshot(self):
        dev = self.devices[0]
        complete = snapshot(dev, sectors_mismatch=8)
        log = "status:\n" + complete + "[time] checkpoint\n"
        truncated = snapshot(dev, sectors_mismatch=1).split("progress_pct=")[0]
        self.assertIn("checksum mismatches=8", self.summary(log, truncated))

    def test_summary_terminal_record_beats_stale_live_snapshot(self):
        dev = self.devices[0]
        log = "status:\n" + snapshot(dev, "done", sectors_mismatch=8)
        log += f"[time] (1/2) {dev}: ISSUES FOUND\n"
        self.assertIn(
            "checksum mismatches=8",
            self.summary(log, snapshot(dev, sectors_mismatch=1)),
        )

    def test_summary_uses_latest_complete_snapshot_once(self):
        dev = self.devices[0]
        log = "status:\n" + snapshot(dev, sectors_mismatch=3) + "[time] checkpoint\n"
        result = self.summary(log, snapshot(dev, sectors_mismatch=8))
        self.assertIn("checksum mismatches=8", result)
        self.assertNotIn("checksum mismatches=11", result)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--mock"]:
        sys.exit(mock_main(sys.argv[2], sys.argv[3:]))
    unittest.main(verbosity=2)
