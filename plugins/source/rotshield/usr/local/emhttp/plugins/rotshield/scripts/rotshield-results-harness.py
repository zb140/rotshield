#!/usr/bin/env python3
"""Isolated integration tests for persistent scrub results."""

import json
import os
import pathlib
import shutil
import signal
import subprocess
import tempfile
import time

SOURCE = pathlib.Path(__file__).resolve().parent

with tempfile.TemporaryDirectory(prefix="rotshield-results-test-") as td:
    root = pathlib.Path(td)
    plugin = root / "plugin"
    config = root / "config"
    runs = config / "runs"
    results = config / "results.json"

    (plugin / "bin").mkdir(parents=True)
    (plugin / "scripts").mkdir()
    runs.mkdir(parents=True)

    for name in (
        "scrub.sh",
        "notify-common.sh",
        "results.php",
        "status-parser.php",
    ):
        shutil.copy2(SOURCE / name, plugin / "scripts" / name)

    runner_path = plugin / "scripts" / "scrub.sh"

    (root / "proc").write_text(
        "rdevName.1=rotshield-test-a\n"
        "rdevId.1=TEST_DISK_A\n"
        "rdevOffset.1=0\n"
        "rdevName.2=rotshield-test-b\n"
        "rdevId.2=TEST_DISK_B\n"
        "rdevOffset.2=0\n"
    )

    fake = plugin / "bin" / "scrub-rs"
    fake.write_text("""#!/bin/bash
device="$1"
case "$MOCK_MODE" in
    error)
        echo "mock scanner error"
        exit 1
        ;;
    cancel)
        echo "mock scanner ready"
        trap 'exit 130' TERM INT
        while :; do sleep 0.2 & wait $!; done
        ;;
esac

echo "scrub complete:"
echo "status:"
echo "state=done"
echo "device=$device"
echo "sectors_checked=1000"
echo "sectors_mismatch=0"
echo "progress_pct=100.00"
exit 0
""")
    fake.chmod(0o755)

    env = dict(
        os.environ,
        ROTSHIELD_PLUGIN_DIR=str(plugin),
        ROTSHIELD_CONFIG_DIR=str(config),
        ROTSHIELD_LOG=str(root / "rotshield.log"),
        ROTSHIELD_NOTIFY_CMD="/bin/true",
        PROC_NMDSTAT=str(root / "proc"),
    )

    def configure(devices):
        (config / "config.cfg").write_text(
            f'DEVICES="{devices}"\n'
            "WRITE=0\n"
            f'EXTRA_OPTIONS="--status-sock {root}/status.sock"\n'
        )

    def command(action, mode="clean", *args):
        return subprocess.run(
            ["bash", str(runner_path), action, *args],
            env=dict(env, MOCK_MODE=mode),
            capture_output=True,
            text=True,
            timeout=30,
        )

    def records():
        return json.loads(results.read_text())["disks"]

    # Test 1: clean sequential scan of two disks.
    configure("/dev/rotshield-test-a /dev/rotshield-test-b")
    result = command("run")
    assert result.returncode == 0, result.stderr

    disks = records()
    assert set(disks) == {"TEST_DISK_A", "TEST_DISK_B"}

    for disk in disks.values():
        assert disk["outcome"] == "clean", disk
        assert disk["counters"]["sectors_checked"] == 1000
        assert disk["started_at"]
        assert disk["finished_at"]

    original_b = disks["TEST_DISK_B"]
    print("PASS: clean sequential scan")

    # Avoid reusing the runner's second-resolution log filename.
    time.sleep(1.1)

    # Test 2: failed scan replaces only the targeted disk.
    configure("/dev/rotshield-test-a")
    result = command("run", "error")
    assert result.returncode == 1, result.stderr

    disks = records()
    assert disks["TEST_DISK_A"]["outcome"] == "error"
    assert disks["TEST_DISK_A"]["counters"] is None
    assert disks["TEST_DISK_B"] == original_b
    print("PASS: error result and preservation of other disk")

    time.sleep(1.1)

    # Test 3: cancellation with a fallback status snapshot.
    runner = subprocess.Popen(
        ["bash", str(runner_path), "run"],
        env=dict(env, MOCK_MODE="cancel"),
        start_new_session=True,
    )

    try:
        for _ in range(100):
            logs = sorted(runs.glob("run-*.log"))
            if logs and "mock scanner ready" in logs[-1].read_text():
                break
            if runner.poll() is not None:
                raise RuntimeError("Runner exited prematurely")
            time.sleep(0.1)
        else:
            raise RuntimeError("Mock scanner did not start")

        snapshot = root / "snapshot"
        snapshot.write_text(
            "state=running\n"
            "device=/dev/rotshield-test-a\n"
            "sectors_checked=250\n"
            "sectors_mismatch=2\n"
            "progress_pct=25.00\n"
        )

        stopped = command("stop", "cancel", str(snapshot))
        print("Stop stdout:", stopped.stdout)
        print("Stop stderr:", stopped.stderr)
        assert stopped.returncode == 0, stopped.stdout + stopped.stderr
        runner.wait(timeout=5)

        disks = records()
        disk = disks["TEST_DISK_A"]

        assert disk["outcome"] == "cancelled", disk
        assert disk["counters"]["sectors_checked"] == 250, disk
        assert disk["counters"]["sectors_mismatch"] == 2, disk
        assert disk["counters"]["progress_pct"] == 25
        assert disks["TEST_DISK_B"] == original_b
        assert not (runs / ".lock").exists()

        print("PASS: cancellation and partial counters")

    finally:
        if runner.poll() is None:
            os.killpg(runner.pid, signal.SIGKILL)
            runner.wait()

    time.sleep(1.1)

    # Test 4: cancellation between disks.
    configure("/dev/rotshield-test-a /dev/rotshield-test-b")

    source = runner_path.read_text()
    anchor = '            rc=$?\n            [ "${rc}" -gt "${overall_rc}" ]'

    assert source.count(anchor) == 1, "Cannot locate between-disk test hook"

    source = source.replace(
        anchor,
        "            rc=$?\n"
        '            if [ "$idx" = 1 ]; then\n'
        '                echo "mock between-disk pause"\n'
        '                while [ ! -e "${LOCK_DIR}/stop-pending" ]; do\n'
        "                    sleep 0.1\n"
        "                done\n"
        "            fi\n"
        '            [ "${rc}" -gt "${overall_rc}" ]',
        1,
    )
    runner_path.write_text(source)

    runner = subprocess.Popen(
        ["bash", str(runner_path), "run"],
        env=dict(env, MOCK_MODE="clean"),
        start_new_session=True,
    )

    try:
        for _ in range(100):
            logs = sorted(runs.glob("run-*.log"))
            if logs and "mock between-disk pause" in logs[-1].read_text():
                break
            if runner.poll() is not None:
                raise RuntimeError("Runner exited prematurely")
            time.sleep(0.1)
        else:
            raise RuntimeError("Between-disk pause not reached")

        before_stop = records()
        assert before_stop["TEST_DISK_A"]["outcome"] == "clean"
        assert before_stop["TEST_DISK_B"] == original_b
        assert (runs / ".lock" / "disk-finalized").exists()

        stopped = command("stop")
        assert stopped.returncode == 0, stopped.stdout + stopped.stderr
        runner.wait(timeout=5)

        log = logs[-1].read_text()

        assert records() == before_stop, "Completed results were modified"
        assert "state=cancelled" not in log, log
        assert "scrubbing /dev/rotshield-test-b" not in log, log
        assert log.rstrip().endswith("finished: CANCELLED (manual stop)")
        assert not (runs / ".lock").exists()

        print("PASS: between-disk cancellation preserves completed results")

    finally:
        if runner.poll() is None:
            os.killpg(runner.pid, signal.SIGKILL)
            runner.wait()

    print("\nFinal results.json:")
    print(results.read_text())
    print("ALL TESTS PASSED")
