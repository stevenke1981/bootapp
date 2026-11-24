"""QEMU acceptance for BootOrder self-registration (menu key F), x64 OVMF, tcg.

OVMF code is attached read-only; NVRAM is a PRIVATE COPY of the vars template
(default `<qemu-dir>/share/edk2-i386-vars.fd`, which QEMU's own firmware
descriptor 60-edk2-x86_64.json names as the x86_64 NVRAM template) inside the
run directory. The template is never opened for writing.

run 1: removable-media fallback boot of BootApp; the startup BootOrder
       (BOOTAPP_BOOT_ORDER, >= 2 entries) is captured; press F; expect
       BOOTAPP_BOOTORDER_SET Boot#### order=... == prepend_unique(startup, ####).
run 2: same private vars, same disk image: the firmware must boot that
       Boot#### itself (BOOTAPP_BOOT_CURRENT ####); its startup BootOrder starts
       with #### and keeps every earlier number. Press F again: the same
       Boot#### is reused and the BootOrder keeps every earlier number.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import time

EXE = ".exe" if os.name == "nt" else ""
SET = re.compile(r"BOOTAPP_BOOTORDER_SET Boot([0-9A-F]{4}) order=([0-9A-F,]*)")
CURRENT = re.compile(r"BOOTAPP_BOOT_CURRENT ([0-9A-F]{4})")
STARTUP_ORDER = re.compile(r"BOOTAPP_BOOT_ORDER ([0-9A-F,]*)")
CONFIG = ("timeout=3600\ndefault=payload\n[entry payload]\ntitle=Acceptance payload\n"
          "path=\\EFI\\BOOTAPP\\payload.efi\n")


def put(volume, path, content):
    destination = volume.joinpath(*path.strip("\\").split("\\"))
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(content)


class Qemu:
    def __init__(self, qemu_dir, code, vars_copy, directory, log, timeout):
        self.log = directory / log
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        self.command = [str(qemu_dir / f"qemu-system-x86_64{EXE}"), "-machine", "q35", "-accel", "tcg",
                        "-m", "256", "-display", "none", "-serial", f"file:{log}", "-monitor", "none",
                        "-nic", "none", "-no-reboot", "-qmp", f"tcp:127.0.0.1:{port},server=on,wait=off",
                        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
                        "-drive", f"if=pflash,format=raw,unit=1,file={vars_copy}",
                        "-drive", "file=esp.img,format=raw,if=ide,index=0,snapshot=on"]
        self.errors = (directory / f"{log}.stderr").open("wb")
        self.process = subprocess.Popen(self.command, cwd=directory, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=self.errors)
        self.connection = None
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"QEMU exited {self.process.returncode}")
            try:
                self.connection = socket.create_connection(("127.0.0.1", port), timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        if self.connection is None:
            raise RuntimeError("QMP startup timeout")
        self.connection.settimeout(5)
        self.wire = self.connection.makefile("rwb")
        json.loads(self.wire.readline())
        self.qmp("qmp_capabilities")

    def qmp(self, name, arguments=None):
        self.wire.write((json.dumps({"execute": name, "arguments": arguments or {}}) + "\n").encode())
        self.wire.flush()
        while True:
            line = self.wire.readline()
            if not line:
                raise RuntimeError("QMP closed unexpectedly")
            response = json.loads(line)
            if "error" in response:
                raise RuntimeError(str(response))
            if "return" in response:
                return response["return"]

    def key(self, name):
        self.qmp("send-key", {"keys": [{"type": "qcode", "data": name}], "hold-time": 30})

    def serial(self):
        return self.log.read_text(encoding="utf-8", errors="replace") if self.log.exists() else ""

    def wait_for(self, predicate, seconds, label):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            text = self.serial()
            if predicate(text):
                return text
            if self.process.poll() is not None:
                raise RuntimeError(f"QEMU exited {self.process.returncode} waiting for {label}")
            time.sleep(0.1)
        raise RuntimeError(f"Timed out waiting for {label}")

    def close(self):
        """QMP quit makes QEMU flush the writable pflash (private vars copy)."""
        try:
            if self.connection:
                try:
                    self.qmp("quit")
                except (OSError, ValueError, RuntimeError):
                    pass
                self.wire.close()
                self.connection.close()
        finally:
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                self.process.wait(timeout=5)
            self.errors.close()


def prepend_unique(order, number):
    """Python model of loadopt::prepend_unique (the BRIEF's BootOrder rule)."""
    return [number] + [item for item in order if item != number]


def press_f(vm, timeout):
    """Return (serial text, startup BootOrder, Boot#### set by F, BootOrder after F)."""
    menu = vm.wait_for(lambda t: "BOOTAPP_MENU entries=" in t, timeout, "menu")
    startup = STARTUP_ORDER.search(menu)
    if not startup:
        raise AssertionError("startup BOOTAPP_BOOT_ORDER trace missing: "
                             + str(re.findall(r"BOOTAPP_BOOT_ORDER_ERROR [^\r\n]*", menu)))
    startup = [item for item in startup.group(1).split(",") if item]
    before = len(vm.serial())
    vm.key("f")
    text = vm.wait_for(lambda t: "BOOTAPP_BOOTORDER_SET" in t[before:] or "BOOTAPP_BOOTORDER_ERROR" in t[before:],
                       30, "BootOrder result")
    match = SET.search(text[before:])
    if not match:
        raise AssertionError(re.search(r"BOOTAPP_BOOTORDER_ERROR[^\r\n]*", text[before:]).group(0))
    number, order = match.group(1), [item for item in match.group(2).split(",") if item]
    if not order or order[0] != number or order.count(number) != 1:
        raise AssertionError(f"Boot{number} is not uniquely first in {order}")
    if order != prepend_unique(startup, number):
        raise AssertionError(f"BootOrder after F {order} != prepend_unique({startup}, {number})")
    return text, startup, number, order


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--qemu-dir", type=Path, default=os.environ.get("BOOTAPP_QEMU_DIR"),
                        required="BOOTAPP_QEMU_DIR" not in os.environ)
    parser.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--out", type=Path, help="run directory parent (default: <project-root>/out)")
    parser.add_argument("--firmware-code", type=Path, help="default: <qemu-dir>/share/edk2-x86_64-code.fd")
    parser.add_argument("--firmware-vars", type=Path, help="vars TEMPLATE (default: <qemu-dir>/share/edk2-i386-vars.fd)")
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--keep-images", action="store_true", help="keep esp.img and the private vars copy")
    args = parser.parse_args()
    root = args.project_root.resolve()
    qemu_dir = Path(args.qemu_dir).resolve()
    code = (args.firmware_code or qemu_dir / "share/edk2-x86_64-code.fd").resolve()
    template = (args.firmware_vars or qemu_dir / "share/edk2-i386-vars.fd").resolve()
    release = root / "target/x86_64-unknown-uefi/release"
    for path in (release / "bootapp-efi.efi", release / "bootapp-payload.efi", code, template,
                 qemu_dir / f"qemu-img{EXE}", qemu_dir / f"qemu-system-x86_64{EXE}"):
        if not path.is_file():
            parser.error(f"Required regular file is missing: {path}")
    directory = (args.out or root / "out").resolve() / f"bootorder-{time.time_ns()}"
    directory.mkdir(parents=True)
    report = {"result": "FAIL", "cwd": str(directory), "checks": [], "vars_template": str(template),
              "limitation": "OVMF in QEMU with a private NVRAM copy; no real firmware NVRAM is touched."}
    template_hash = hashlib.sha256(template.read_bytes()).hexdigest()
    vars_copy = directory / "vars.fd"
    shutil.copyfile(template, vars_copy)
    vm = None
    try:
        put(directory / "esp", r"\EFI\BOOT\BOOTX64.EFI", (release / "bootapp-efi.efi").read_bytes())
        put(directory / "esp", r"\EFI\BOOTAPP\payload.efi", (release / "bootapp-payload.efi").read_bytes())
        put(directory / "esp", r"\EFI\BOOTAPP\bootapp.cfg", CONFIG.encode())
        subprocess.run([str(qemu_dir / f"qemu-img{EXE}"), "convert", "-f", "vvfat", "fat:esp", "-O", "raw",
                        "esp.img"], cwd=directory, check=True, timeout=60)
        shutil.rmtree(directory / "esp")

        vm = Qemu(qemu_dir, code, vars_copy, directory, "run1.log", args.timeout)
        report["command_run1"] = vm.command
        text, startup1, number, order = press_f(vm, args.timeout)
        first_current = CURRENT.search(text)
        report["run1"] = {"boot_current": first_current and first_current.group(1), "startup_order": startup1,
                          "set": number, "order": order}
        if first_current and first_current.group(1) == number:
            raise AssertionError("run 1 already booted the number it created; test would prove nothing")
        if len(startup1) < 2 or number in startup1:
            raise AssertionError(f"run 1 startup BootOrder {startup1} cannot prove prepend semantics")
        report["checks"].append({"name": "run1-F-sets-BootOrder", "result": "PASS", "boot": number, "order": order})
        vm.close()
        vm = None

        vm = Qemu(qemu_dir, code, vars_copy, directory, "run2.log", args.timeout)
        report["command_run2"] = vm.command
        text, startup2, again, order2 = press_f(vm, args.timeout)
        current = CURRENT.search(text)
        report["run2"] = {"boot_current": current and current.group(1), "startup_order": startup2,
                          "set": again, "order": order2}
        if not startup2 or startup2[0] != number or set(order) - set(startup2):
            raise AssertionError(f"run 2 startup BootOrder {startup2} lost Boot{number} first or earlier {order}")
        report["checks"].append({"name": "run2-startup-BootOrder-persisted", "result": "PASS",
                                 "startup_order": startup2})
        if not current or current.group(1) != number:
            raise AssertionError(f"firmware booted {current and current.group(1)}, expected Boot{number}")
        report["checks"].append({"name": "run2-firmware-boots-registered-option", "result": "PASS",
                                 "boot_current": number})
        if again != number or set(order) - set(order2):
            raise AssertionError(f"second F did not reuse Boot{number} or dropped entries: {order} -> {order2}")
        report["checks"].append({"name": "run2-F-reuses-entry-and-keeps-others", "result": "PASS"})
        if hashlib.sha256(template.read_bytes()).hexdigest() != template_hash:
            raise AssertionError("vars template changed")
        report["checks"].append({"name": "shared-vars-template-unchanged", "result": "PASS"})
        report["result"] = "PASS"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        if vm:
            vm.close()
        for log in ("run1.log", "run2.log"):
            if (directory / log).exists():
                report[log] = (directory / log).read_text(encoding="utf-8", errors="replace")
        if not args.keep_images:
            for name in ("esp.img", "vars.fd"):
                if (directory / name).exists():
                    (directory / name).unlink()
        result_path = directory / "result.json"
        result_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps({"result": report["result"], "report": str(result_path), "error": report.get("error"),
                          "run1": report.get("run1"), "run2": report.get("run2")}, indent=2))
    return 0 if report["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
