"""Exercise real QEMU auto-discovery and volume-bound EFI chainloading.

These are synthetic EFI application path fixtures, NOT installed operating
systems and NOT evidence that Windows, Linux or macOS booted. No config file is
created. Only generated regular image files are attached, with snapshot writes.
"""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import shutil
import socket
import subprocess
import time


WINDOWS = r"\EFI\Microsoft\Boot\bootmgfw.efi"
LINUX = r"\EFI\acme\grubx64.efi"
MAC = r"\System\Library\CoreServices\boot.efi"
PREBOOT = r"\01234567-89ab-cdef-0123-456789abcdef\System\Library\CoreServices\boot.efi"
RECOVERY = r"\com.apple.recovery.boot\boot.efi"
APPLE = r"\EFI\APPLE\fixture\boot.efi"
VALID_PATHS = [WINDOWS, LINUX, MAC, PREBOOT, RECOVERY, APPLE]
WRONG_ARCH = r"\EFI\wrong\grubx64.efi"
TRUNCATED = r"\EFI\broken\shimx64.efi"
UNCLASSIFIED = r"\EFI\arbitrary\boot.efi"
BAD_UUID = r"\not-a-uuid\System\Library\CoreServices\boot.efi"
MARKER_ONE = "BOOTAPP_VOL_ONE_OK"
MARKER_TWO = "BOOTAPP_VOL_TWO_OK"
DETECTED = re.compile(r"BOOTAPP_DETECTED ([^\r\n]+?) volume=(\d+) handle=([^\r\n]+?) path=([^\r\n]+)")


def put(volume, path, content):
    destination = volume.joinpath(*path.strip("\\").split("\\"))
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(content)


def marked_payload(source, marker):
    original = "BOOTAPP_PAYLOAD_OK"
    if len(marker) != len(original):
        raise ValueError("Fixture marker must preserve PE byte length")
    replacements = 0
    for encoding in ("ascii", "utf-16-le"):
        old, new = original.encode(encoding), marker.encode(encoding)
        replacements += source.count(old)
        source = source.replace(old, new)
    if not replacements:
        raise ValueError("Acceptance payload does not contain its expected marker")
    return source, replacements


def log_text(directory):
    path = directory / "serial.log"
    return path.read_text(encoding="utf-8", errors="replace") if path.exists() else ""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu-dir", required=True, type=Path)
    parser.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    root = args.project_root.resolve()
    qemu_dir = args.qemu_dir.resolve()
    manager = root / "target/x86_64-unknown-uefi/release/bootapp-efi.efi"
    payload = root / "target/x86_64-unknown-uefi/release/bootapp-payload.efi"
    foreign = root / "target/aarch64-unknown-uefi/release/bootapp-payload.efi"
    for path in (manager, payload, foreign, qemu_dir / "qemu-img.exe", qemu_dir / "qemu-system-x86_64.exe"):
        if not path.is_file():
            parser.error(f"Required regular file is missing: {path}")
    directory = root / "evidence/raw" / f"autodiscovery-{time.time_ns()}"
    directory.mkdir(parents=True, exist_ok=False)
    report = {
        "result": "FAIL", "fixture_only": True,
        "limitation": "Synthetic PE path fixtures; no operating system or APFS driver boot is tested.",
        "cwd": str(directory), "manager_sha256": hashlib.sha256(manager.read_bytes()).hexdigest(),
        "checks": [], "selections": [],
    }
    process = connection = wire = None
    command = []

    def qmp(name, arguments=None):
        wire.write((json.dumps({"execute": name, "arguments": arguments or {}}) + "\n").encode())
        wire.flush()
        while True:
            line = wire.readline()
            if not line:
                raise RuntimeError("QMP closed unexpectedly")
            response = json.loads(line)
            if "error" in response:
                raise RuntimeError(str(response))
            if "return" in response:
                return response["return"]

    def key(name):
        qmp("send-key", {"keys": [{"type": "qcode", "data": name}], "hold-time": 30})
        time.sleep(0.08)

    def wait_for(predicate, seconds, label):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"QEMU exited {process.returncode} waiting for {label}")
            serial = log_text(directory)
            if predicate(serial):
                return serial
            time.sleep(0.1)
        raise RuntimeError(f"Timed out waiting for {label}")

    try:
        one, replacements_one = marked_payload(payload.read_bytes(), MARKER_ONE)
        two, replacements_two = marked_payload(payload.read_bytes(), MARKER_TWO)
        put(directory / "manager", r"\EFI\BOOT\BOOTX64.EFI", manager.read_bytes())
        for path in VALID_PATHS:
            put(directory / "volume-one", path, one)
        put(directory / "volume-two", LINUX, two)
        put(directory / "volume-one", WRONG_ARCH, foreign.read_bytes())
        put(directory / "volume-one", TRUNCATED, b"MZ\0\0truncated")
        put(directory / "volume-one", UNCLASSIFIED, one)
        put(directory / "volume-one", BAD_UUID, one)
        report["marker_replacements"] = [replacements_one, replacements_two]
        report["fixture_sha256"] = {
            "volume_one_payload": hashlib.sha256(one).hexdigest(),
            "volume_two_payload": hashlib.sha256(two).hexdigest(),
            "foreign_payload": hashlib.sha256(foreign.read_bytes()).hexdigest(),
        }
        for name in ("manager", "volume-one", "volume-two"):
            subprocess.run([str(qemu_dir / "qemu-img.exe"), "convert", "-f", "vvfat", f"fat:{name}",
                            "-O", "raw", f"{name}.img"], cwd=directory, check=True, timeout=45)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        command = [str(qemu_dir / "qemu-system-x86_64.exe"), "-name", "bootapp-autodiscovery-fixtures",
                   "-machine", "q35", "-accel", "tcg", "-m", "256", "-display", "none",
                   "-serial", "file:serial.log", "-monitor", "none", "-nic", "none", "-no-reboot",
                   "-qmp", f"tcp:127.0.0.1:{port},server=on,wait=off", "-drive",
                   f"if=pflash,format=raw,readonly=on,file={qemu_dir / 'share/edk2-x86_64-code.fd'}"]
        for index, name in enumerate(("manager", "volume-one", "volume-two")):
            command += ["-drive", f"file={name}.img,format=raw,if=ide,index={index},snapshot=on"]
        report["command"] = command
        with (directory / "stderr.log").open("wb") as errors:
            process = subprocess.Popen(command, cwd=directory, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=errors)
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError((directory / "stderr.log").read_text(errors="replace"))
            try:
                connection = socket.create_connection(("127.0.0.1", port), timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        if connection is None:
            raise RuntimeError("QMP startup timeout")
        connection.settimeout(5)
        wire = connection.makefile("rwb")
        json.loads(wire.readline())
        qmp("qmp_capabilities")
        serial = wait_for(lambda text: "BOOTAPP_MENU entries=" in text, args.timeout, "auto-discovery menu")
        key("esc")
        if "BOOTAPP_AUTO_MODE" not in serial:
            raise AssertionError("Missing-config auto mode marker was not observed")
        detected = [{"os": m[1], "volume_index": int(m[2]), "handle": m[3], "path": m[4]}
                    for m in DETECTED.finditer(serial)]
        report["detected"] = detected
        expected = Counter(path.lower() for path in VALID_PATHS + [LINUX])
        observed = Counter(item["path"].lower() for item in detected)
        if observed != expected:
            raise AssertionError(f"Candidate mismatch: expected={expected}, observed={observed}")
        for item in detected:
            expected_os = "Windows" if item["path"].lower() == WINDOWS.lower() else (
                "Linux" if item["path"].lower() == LINUX.lower() else "macOS")
            if item["os"] != expected_os:
                raise AssertionError(f"Incorrect OS label: {item}")
        windows_handle = next(item["handle"] for item in detected if item["path"].lower() == WINDOWS.lower())
        linux_items = [item for item in detected if item["path"].lower() == LINUX.lower()]
        if len({item["handle"] for item in linux_items}) != 2:
            raise AssertionError("Same paths on two volumes were collapsed")
        if len({item["handle"] for item in detected}) != 2:
            raise AssertionError("Unexpected fixture volume count")
        report["checks"] += [
            {"name": "missing-config-auto-mode", "result": "PASS"},
            {"name": "Windows-Linux-macOS-path-classification", "result": "PASS"},
            {"name": "UUID-Preboot-and-recovery-paths", "result": "PASS"},
            {"name": "wrong-architecture-truncated-and-unrecognized-paths-rejected", "result": "PASS"},
            {"name": "same-path-distinct-volumes-preserved", "result": "PASS"},
        ]
        # Test every item. A different executable marker on each volume proves
        # device binding at execution, independently of the pre-start trace.
        current = 0
        for index, item in enumerate(detected):
            before = len(log_text(directory))
            for _ in range(index - current):
                key("down")
            marker = MARKER_ONE if item["handle"] == windows_handle else MARKER_TWO
            key("ret")
            serial = wait_for(lambda text: marker in text[before:] and "BOOTAPP_CHILD_RETURN Ok(())" in text[before:],
                              20, f"entry {index} execution and return")
            tail = serial[before:]
            starts = re.findall(r"BOOTAPP_START ([^\r\n]+?) volume=([^\r\n]+)", tail)
            if not starts or starts[-1] != (item["path"], item["handle"]):
                raise AssertionError(f"Started target differs from selected detection: {item}, starts={starts}")
            other = MARKER_TWO if marker == MARKER_ONE else MARKER_ONE
            if other in tail:
                raise AssertionError(f"Wrong volume executed for {item}")
            report["selections"].append({**item, "menu_index": index, "execution_marker": marker, "result": "PASS"})
            current = index
        qmp("screendump", {"filename": str(directory / "screen.ppm")})
        report["checks"].append({"name": "all-seven-volume-bound-chainloads-and-returns", "result": "PASS"})
        report["result"] = "PASS"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        if wire:
            try:
                qmp("quit")
            except (OSError, ValueError, RuntimeError):
                pass
            wire.close()
        if connection:
            connection.close()
        if process:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
            report["process_exit_code"] = process.returncode
        report["serial"] = log_text(directory)
        result_path = directory / "result.json"
        result_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps({"result": report["result"], "report": str(result_path),
                          "error": report.get("error"), "fixture_only": True}, indent=2))
    return 0 if report["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
