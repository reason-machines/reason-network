"""macOS tray memory stays bounded while the directory and menus keep changing.

    cargo build --locked
    python3 tests/e2e/tray_memory.py --output /tmp/tray-memory.json

Requires a logged-in macOS desktop and permission to inspect a child with vmmap.
Runs the real tray for about two minutes against a synthetic local IPC server;
no team, daemon, credentials or agent configuration is used. A temporary menu-bar
icon appears during the check. The JSON output records every memory sample.

Keep the menu closed during measurement. This checks memory growth, not UI
progress: the background poller can run while AppKit is stalled. Separately open
a populated submenu, close it, let updates pass, then reopen it and verify that
the synthetic update number changed and that the items still work.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import subprocess
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
MIB = 1024 * 1024


def directory(tick):
    machines, sessions = [], []
    for m in range(8):
        machine = f"box-{m}"
        machines.append(dict(person="test", machine=machine, endpoint=str(m), is_me=m == 0,
                             online=m == 0 or tick % 6 != 0, link="direct", rtt_ms=tick,
                             last_seen=tick, verified=False, has_proof=False))
        for s in range(7 + tick % 2):
            sessions.append(dict(address=f"test/task-{m}-{s}@{machine}", name=f"task-{m}-{s}",
                                 person="test", machine=machine, agent="codex", cwd="/synthetic",
                                 repo="synthetic", branch="main", status="busy" if tick % 2 else "idle",
                                 since=tick, doing=f"Synthetic update {tick}", tier="pull", unread=0,
                                 is_me=False))
    return dict(team="Tray memory test", person="test", machine="box-0", machines=machines, sessions=sessions)


def footprint(pid):
    result = subprocess.run(["/usr/bin/vmmap", "-summary", str(pid)], capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError(result.stderr or result.stdout)
    match = re.search(r"^Physical footprint:\s+([\d.]+)([KMGT]?)", result.stdout, re.M)
    if not match:
        raise RuntimeError("vmmap did not report the child's physical footprint")
    scale = {"": 1, "K": 1024, "M": MIB, "G": 1024 * MIB, "T": 1024 * 1024 * MIB}
    return round(float(match[1]) * scale[match[2]] / MIB, 3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/reason-network")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=120, help="measurement time after a 15-second warmup")
    parser.add_argument("--max-growth-mib", type=float, default=4)
    args = parser.parse_args()
    if platform.system() != "Darwin":
        parser.error("this check requires a macOS desktop")
    if args.seconds < 60 or args.seconds % 30 or args.max_growth_mib <= 0:
        parser.error("use at least 60 seconds in multiples of 30 and a positive memory-growth limit")
    binary = args.binary.resolve(strict=True)
    report = dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  macos=platform.mac_ver()[0], seconds=args.seconds,
                  max_growth_mib=args.max_growth_mib, samples=[], memory_check_passed=False,
                  ui_verification="requires a separate submenu interaction check")
    polls, errors = [], []
    stop = threading.Event()
    process = None
    # A short socket path avoids the production path-length fallback.
    with tempfile.TemporaryDirectory(prefix="rn-tray-", dir="/tmp") as temporary:
        with socket.socket(socket.AF_UNIX) as server, tempfile.TemporaryFile(mode="w+") as log:
            socket_path = os.path.join(temporary, "network.sock")
            server.bind(socket_path)
            os.chmod(socket_path, 0o600)
            server.listen(1)
            server.settimeout(0.5)

            def serve():
                try:
                    while not stop.is_set():
                        try:
                            conn, _ = server.accept()
                        except socket.timeout:
                            continue
                        with conn, conn.makefile("rb") as stream:
                            for line in stream:
                                request = json.loads(line)
                                if request.get("op") != "who":
                                    raise RuntimeError(f"unexpected request: {request.get('op')}")
                                tick = len(polls) + 1
                                reply = dict(id=request["id"], ok=True, result=directory(tick))
                                conn.sendall((json.dumps(reply) + "\n").encode())
                                polls.append(time.monotonic())
                except (BrokenPipeError, ConnectionResetError):
                    if not stop.is_set():
                        errors.append("tray disconnected during the check")
                except Exception as error:
                    errors.append(str(error))

            worker = threading.Thread(target=serve, daemon=True)
            worker.start()
            env = {k: v for k, v in os.environ.items() if not k.startswith(("REASON_NETWORK_", "TINCAN_"))}
            env.update(REASON_NETWORK_HOME=temporary, REASON_NETWORK_NO_SERVICE="1",
                       REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_MDNS="1")
            try:
                process = subprocess.Popen([str(binary), "tray"], env=env, cwd=temporary,
                                           stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                start = time.monotonic()
                for offset in range(0, args.seconds + 1, 30):
                    deadline = start + 15 + offset
                    while time.monotonic() < deadline:
                        if process.poll() is not None or errors:
                            raise RuntimeError(f"tray exited early ({process.poll()}), server errors: {errors}")
                        time.sleep(min(0.2, max(0, deadline - time.monotonic())))
                    sample = dict(elapsed_seconds=round(time.monotonic() - start, 2),
                                  footprint_mib=footprint(process.pid), polls=len(polls))
                    report["samples"].append(sample)
                    print(json.dumps(sample), flush=True)
                samples = report["samples"]
                report["growth_mib"] = round(max(s["footprint_mib"] for s in samples[1:])
                                             - samples[0]["footprint_mib"], 3)
                report["measured_polls"] = samples[-1]["polls"] - samples[0]["polls"]
                report["memory_check_passed"] = (report["growth_mib"] <= args.max_growth_mib
                                                and report["measured_polls"] >= args.seconds // 3
                                                and process.poll() is None and not errors)
            except Exception as error:
                report["error"] = str(error)
            finally:
                stop.set()
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                worker.join(timeout=2)
                log.seek(0)
                report["tray_log_tail"] = log.read()[-4000:]
                report["server_errors"] = errors
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"{'PASS' if report['memory_check_passed'] else 'FAIL'} (memory only): {args.output}")
    return 0 if report["memory_check_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
