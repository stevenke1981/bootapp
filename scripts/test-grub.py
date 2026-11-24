"""QEMU acceptance for the UEFI manager's GRUB grub.cfg subset (x64, OVMF, tcg).

Runs (each a fresh QEMU with snapshot=on drives, generated images only):
  main        \\EFI\\BOOTAPP\\grub.cfg only (no bootapp.cfg):
              (a) chainloader on the manager's own volume
              (b) search --file --set=root to a second FAT volume
              (c) search --fs-uuid (serial patched into, then read back from, the
                  second volume's boot sector before grub.cfg is written)
              (d) search --label (vvfat `label` option; read back from the image)
              (e) Linux EFI-stub style entry: \\vmlinuz-test (no .efi) with linux
                  args + two initrds; BOOTAPP_PAYLOAD_OPTIONS is asserted exactly
              (f) set timeout=-1: no countdown, no automatic boot; Enter via QMP
              (+) unresolvable fs-uuid entry stays listed and fails cleanly
              (+) insmod is accepted with a BOOTAPP_GRUB_WARNING
  invalid     (g) invalid grub.cfg -> BOOTAPP_CONFIG_ERROR naming the line
  precedence  bootapp.cfg present -> grub.cfg (even an invalid one) is not read

The payloads are BootApp's own acceptance fixture with a marker substituted to
prove which volume executed; no operating system or real kernel is booted.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import subprocess
import time

EXE = ".exe" if os.name == "nt" else ""
MARKER_ONE = "BOOTAPP_VOL_ONE_OK"
MARKER_TWO = "BOOTAPP_VOL_TWO_OK"
PROBE = r"\EFI\SHARED\probe.efi"
TAG = r"\EFI\TWO\tag.txt"
KERNEL = r"\vmlinuz-test"
LABEL = "BAPPTWO"
PATCHED_SERIAL = 0x20261124
EXPECTED_OPTIONS = r"console=ttyS0 quiet initrd=\initrd-a.img initrd=\initrd-b.img"
ANSI = re.compile(r"\x1b\[[0-9;=?]*[A-Za-z]")


def put(volume, path, content):
    destination = volume.joinpath(*path.strip("\\").split("\\"))
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(content)


def marked_payload(source, marker):
    """Same-length marker substitution (see test-autodiscovery.py)."""
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
    return source


def partition_offset(image):
    with image.open("rb") as handle:
        mbr = handle.read(512)
    if mbr[510:512] != b"\x55\xaa":
        raise ValueError(f"{image} has no MBR signature")
    start = struct.unpack_from("<I", mbr, 0x1BE + 8)[0]
    if start == 0:
        raise ValueError(f"{image} first partition entry is empty")
    return start * 512


def read_fat_serial(image):
    """GRUB-style fs-uuid of the first partition: FAT32 at 0x43 when
    BPB_FATSz16 == 0, else FAT12/16 at 0x27; printed %04X-%04X."""
    offset = partition_offset(image)
    with image.open("rb") as handle:
        handle.seek(offset)
        sector = handle.read(512)
    if sector[510:512] != b"\x55\xaa":
        raise ValueError("partition boot sector is not signed")
    fat_sz16 = struct.unpack_from("<H", sector, 0x16)[0]
    serial = struct.unpack_from("<I", sector, 0x43 if fat_sz16 == 0 else 0x27)[0]
    return f"{serial >> 16:04X}-{serial & 0xFFFF:04X}", ("FAT32" if fat_sz16 == 0 else "FAT12/16")


def patch_fat16_serial(image, serial):
    """vvfat stamps every volume with the same fixed serial; give the second
    volume a unique one so --fs-uuid is unambiguous. Generated file only."""
    offset = partition_offset(image)
    with image.open("r+b") as handle:
        handle.seek(offset)
        sector = handle.read(512)
        if struct.unpack_from("<H", sector, 0x16)[0] == 0:
            raise ValueError("expected a FAT12/16 vvfat volume")
        handle.seek(offset + 0x27)
        handle.write(struct.pack("<I", serial))


def read_bpb_label(image):
    offset = partition_offset(image)
    with image.open("rb") as handle:
        handle.seek(offset)
        sector = handle.read(512)
    fat_sz16 = struct.unpack_from("<H", sector, 0x16)[0]
    start = 0x47 if fat_sz16 == 0 else 0x2B
    return sector[start:start + 11].decode("ascii", "replace").rstrip()


class Qemu:
    def __init__(self, qemu_dir, code, directory, drives, timeout):
        self.directory = directory
        self.timeout = timeout
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        self.command = [str(qemu_dir / f"qemu-system-x86_64{EXE}"), "-machine", "q35", "-accel", "tcg",
                        "-m", "256", "-display", "none", "-serial", "file:serial.log", "-monitor", "none",
                        "-nic", "none", "-no-reboot", "-qmp", f"tcp:127.0.0.1:{port},server=on,wait=off",
                        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}"]
        for index, name in enumerate(drives):
            self.command += ["-drive", f"file={name},format=raw,if=ide,index={index},snapshot=on"]
        self.errors = (directory / "stderr.log").open("wb")
        self.process = subprocess.Popen(self.command, cwd=directory, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=self.errors)
        self.connection = None
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError((directory / "stderr.log").read_text(errors="replace"))
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
        time.sleep(0.08)

    def serial(self):
        path = self.directory / "serial.log"
        return path.read_text(encoding="utf-8", errors="replace") if path.exists() else ""

    def wait_for(self, predicate, seconds, label, allow_exit=False):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            text = self.serial()
            if predicate(text):
                return text
            if self.process.poll() is not None and not allow_exit:
                raise RuntimeError(f"QEMU exited {self.process.returncode} waiting for {label}")
            time.sleep(0.1)
        raise RuntimeError(f"Timed out waiting for {label}")

    def close(self):
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
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                self.process.wait(timeout=5)
            self.errors.close()


def convert(qemu_dir, directory, name, label=None):
    source = (f'json:{{"driver":"vvfat","dir":"{name}","label":"{label}"}}' if label else f"fat:{name}")
    subprocess.run([str(qemu_dir / f"qemu-img{EXE}"), "convert", *([] if label else ["-f", "vvfat"]), source,
                    "-O", "raw", f"{name}.img"], cwd=directory, check=True, timeout=60)
    shutil.rmtree(directory / name)
    return directory / f"{name}.img"


def write_grub(directory, text):
    put(directory / "manager", r"\EFI\BOOTAPP\grub.cfg", text.encode("utf-8"))


MAIN_GRUB = """# BootApp test-grub fixture (generated)
set timeout=-1
set default=0
insmod part_msdos
menuentry "A own-volume chainloader" --id own {{
    chainloader /EFI/SHARED/probe.efi
}}
menuentry "B search --file" --id file {{
    search --no-floppy --file --set=root /EFI/TWO/tag.txt
    chainloader ($root)/EFI/SHARED/probe.efi
}}
menuentry "C search --fs-uuid" --id uuid {{
    search --no-floppy --fs-uuid --set=root {serial}
    chainloader /EFI/SHARED/probe.efi
}}
menuentry "D search --label" --id label {{
    search --label --set=root {label}
    chainloader /EFI/SHARED/probe.efi
}}
menuentry "E Linux EFI stub" --id linux {{
    linux /vmlinuz-test console=ttyS0 quiet
    initrd /initrd-a.img /initrd-b.img
}}
menuentry "F unresolvable volume" --id missing {{
    search --fs-uuid --set=root 0000-0000
    chainloader /EFI/SHARED/probe.efi
}}
"""
INSMOD_LINE = 4

INVALID_GRUB = """set timeout=5
menuentry "bad device name" {
    chainloader (hd0,gpt1)/EFI/x.efi
}
"""
INVALID_LINE = 3


def run_main(args, base, binaries, report):
    directory = base / "main"
    directory.mkdir()
    one = marked_payload(binaries["payload"], MARKER_ONE)
    two = marked_payload(binaries["payload"], MARKER_TWO)
    put(directory / "volume-two", PROBE, two)
    put(directory / "volume-two", TAG, b"volume two\n")
    two_image = convert(args.qemu_dir, directory, "volume-two", LABEL)
    patch_fat16_serial(two_image, PATCHED_SERIAL)
    serial_text, fat_type = read_fat_serial(two_image)
    label = read_bpb_label(two_image)
    if label != LABEL:
        raise AssertionError(f"vvfat label option not honoured: {label!r}")
    report["volume_two"] = {"fs_uuid": serial_text, "fat_type": fat_type, "label": label}
    manager = directory / "manager"
    put(manager, r"\EFI\BOOT\BOOTX64.EFI", binaries["manager"])
    put(manager, PROBE, one)
    put(manager, KERNEL, one)
    put(manager, r"\initrd-a.img", b"initrd-a\n")
    put(manager, r"\initrd-b.img", b"initrd-b\n")
    write_grub(directory, MAIN_GRUB.format(serial=serial_text, label=label))
    convert(args.qemu_dir, directory, "manager")
    vm = Qemu(args.qemu_dir, args.code, directory, ["manager.img", "volume-two.img"], args.timeout)
    report["runs"]["main"] = {"command": vm.command, "cwd": str(directory)}
    checks = []
    try:
        text = vm.wait_for(lambda t: "BOOTAPP_MENU entries=" in t, args.timeout, "menu")
        if "BOOTAPP_CONFIG_SOURCE grub.cfg" not in text:
            raise AssertionError("grub.cfg was not the config source")
        menu = re.search(r"BOOTAPP_MENU entries=(\d+) default=(\d+) timeout=(\S+)", text)
        if menu.groups() != ("6", "0", "disabled"):
            raise AssertionError(f"unexpected menu line: {menu.group(0)}")
        if f"BOOTAPP_GRUB_WARNING line={INSMOD_LINE} " not in text:
            raise AssertionError("insmod warning was not traced")
        own = int(re.search(r"BOOTAPP_OWN_VOLUME volume=(\d+)", text).group(1))
        checks.append({"name": "grub.cfg-source-warning-and-entries", "result": "PASS"})
        # (f) timeout=-1: wait well past any countdown; nothing may start.
        time.sleep(6)
        text = vm.serial()
        plain = ANSI.sub("", text)
        if "BOOTAPP_START" in text or "Automatic boot in" in plain:
            raise AssertionError("timeout=-1 still counted down or booted")
        if "Automatic boot disabled." not in plain:
            raise AssertionError("'Automatic boot disabled.' not rendered on the serial console")
        checks.append({"name": "f-timeout-disabled-no-countdown", "result": "PASS"})
        expectations = [
            ("own", MARKER_ONE, False, None),
            ("file", MARKER_TWO, True, None),
            ("uuid", MARKER_TWO, True, None),
            ("label", MARKER_TWO, True, None),
            ("linux", MARKER_ONE, False, EXPECTED_OPTIONS),
        ]
        for index, (entry_id, marker, resolved, options) in enumerate(expectations):
            before = len(vm.serial())
            if index:
                vm.key("down")
            vm.key("ret")
            tail = vm.wait_for(lambda t: "BOOTAPP_CHILD_RETURN" in t[before:], 30, f"entry {entry_id}")[before:]
            other = MARKER_TWO if marker == MARKER_ONE else MARKER_ONE
            if f"BOOTAPP_CHILD_RETURN Ok(())" not in tail or marker not in tail or other in tail:
                raise AssertionError(f"entry {entry_id}: wrong or failed execution:\n{ANSI.sub('', tail)}")
            if resolved:
                match = re.search(rf"BOOTAPP_VOLUME_RESOLVED id={entry_id} volume=(\d+)", tail)
                if not match or int(match.group(1)) == own:
                    raise AssertionError(f"entry {entry_id}: not resolved to the second volume")
            got = re.search(r"BOOTAPP_PAYLOAD_OPTIONS ([^\r\n]*)", tail)
            got = got.group(1) if got else None
            if options is not None and got != options:
                raise AssertionError(f"entry {entry_id}: options {got!r} != {options!r}")
            if options is None and got != "":
                raise AssertionError(f"entry {entry_id}: unexpected options {got!r}")
            checks.append({"name": {"own": "a-own-volume-chainloader", "file": "b-search-file",
                                    "uuid": "c-search-fs-uuid", "label": "d-search-label",
                                    "linux": "e-linux-efi-stub-options-initrd"}[entry_id],
                           "result": "PASS", "marker": marker, "options": got})
        before = len(vm.serial())
        vm.key("down")
        vm.key("ret")
        tail = vm.wait_for(lambda t: "Boot failed: volume not found" in t[before:], 30, "unresolvable entry")[before:]
        if "BOOTAPP_VOLUME_NOT_FOUND id=missing fs-uuid=0000-0000" not in tail or "BOOTAPP_START" in tail:
            raise AssertionError("unresolvable entry did not fail cleanly")
        vm.key("up")
        before = len(vm.serial())
        vm.key("ret")
        vm.wait_for(lambda t: MARKER_ONE in t[before:] and "BOOTAPP_CHILD_RETURN Ok(())" in t[before:],
                    30, "menu still usable after failure")
        checks.append({"name": "unresolved-volume-listed-and-fails-cleanly", "result": "PASS"})
        vm.qmp("screendump", {"filename": str(directory / "screen.ppm")})
    finally:
        vm.close()
        report["runs"]["main"]["serial"] = vm.serial()
    return checks


def run_simple(args, base, binaries, name, grub, bootapp_cfg, report):
    directory = base / name
    directory.mkdir()
    manager = directory / "manager"
    put(manager, r"\EFI\BOOT\BOOTX64.EFI", binaries["manager"])
    put(manager, r"\EFI\BOOTAPP\payload.efi", binaries["payload"])
    write_grub(directory, grub)
    if bootapp_cfg:
        put(manager, r"\EFI\BOOTAPP\bootapp.cfg", bootapp_cfg.encode())
    convert(args.qemu_dir, directory, "manager")
    vm = Qemu(args.qemu_dir, args.code, directory, ["manager.img"], args.timeout)
    report["runs"][name] = {"command": vm.command, "cwd": str(directory)}
    try:
        if name == "invalid":
            text = vm.wait_for(lambda t: "BOOTAPP_CONFIG_ERROR" in t, args.timeout, "config error", allow_exit=True)
            line = re.search(r"BOOTAPP_CONFIG_ERROR ([^\r\n]*)", text).group(1)
            if not line.startswith(f"grub.cfg line {INVALID_LINE}:") or "BOOTAPP_MENU" in text:
                raise AssertionError(f"unexpected config error: {line!r}")
            return [{"name": "g-invalid-grub-cfg-error-with-line", "result": "PASS", "error": line}]
        text = vm.wait_for(lambda t: "BOOTAPP_MENU entries=" in t, args.timeout, "menu")
        if ("BOOTAPP_CONFIG_SOURCE bootapp.cfg" not in text or "grub.cfg" in text.split("BOOTAPP_MENU")[0]
                or "BOOTAPP_CONFIG_ERROR" in text or "BOOTAPP_MENU entries=1 default=0 timeout=3600" not in text):
            raise AssertionError("bootapp.cfg did not take precedence over grub.cfg")
        return [{"name": "bootapp-cfg-precedence-over-grub-cfg", "result": "PASS"}]
    finally:
        vm.close()
        report["runs"][name]["serial"] = vm.serial()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--qemu-dir", type=Path, default=os.environ.get("BOOTAPP_QEMU_DIR"),
                        required="BOOTAPP_QEMU_DIR" not in os.environ)
    parser.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--out", type=Path, help="run directory parent (default: <project-root>/out)")
    parser.add_argument("--firmware-code", type=Path, help="default: <qemu-dir>/share/edk2-x86_64-code.fd")
    parser.add_argument("--firmware-vars", type=Path, help="accepted for interface parity; not needed (no NVRAM writes)")
    parser.add_argument("--case", choices=["all", "main", "invalid", "precedence"], default="all")
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--keep-images", action="store_true", help="keep generated *.img files")
    args = parser.parse_args()
    root = args.project_root.resolve()
    args.qemu_dir = Path(args.qemu_dir).resolve()
    args.code = (args.firmware_code or args.qemu_dir / "share/edk2-x86_64-code.fd").resolve()
    release = root / "target/x86_64-unknown-uefi/release"
    for path in (release / "bootapp-efi.efi", release / "bootapp-payload.efi", args.code,
                 args.qemu_dir / f"qemu-img{EXE}", args.qemu_dir / f"qemu-system-x86_64{EXE}"):
        if not path.is_file():
            parser.error(f"Required regular file is missing: {path}")
    base = (args.out or root / "out").resolve() / f"grub-{time.time_ns()}"
    base.mkdir(parents=True)
    binaries = {"manager": (release / "bootapp-efi.efi").read_bytes(),
                "payload": (release / "bootapp-payload.efi").read_bytes()}
    report = {"result": "FAIL", "fixture_only": True, "cwd": str(base), "checks": [], "runs": {},
              "limitation": "Payloads are BootApp's PE fixture; no real Linux kernel or initrd is executed."}
    try:
        if args.case in ("all", "main"):
            report["checks"] += run_main(args, base, binaries, report)
        if args.case in ("all", "invalid"):
            report["checks"] += run_simple(args, base, binaries, "invalid", INVALID_GRUB, None, report)
        if args.case in ("all", "precedence"):
            report["checks"] += run_simple(
                args, base, binaries, "precedence", INVALID_GRUB,
                "timeout=3600\ndefault=p\n[entry p]\ntitle=Payload\npath=\\EFI\\BOOTAPP\\payload.efi\n", report)
        report["result"] = "PASS"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        if not args.keep_images:
            for image in base.glob("*/*.img"):
                image.unlink()
        result_path = base / "result.json"
        result_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps({"result": report["result"], "report": str(result_path), "error": report.get("error"),
                          "checks": [(c["name"], c["result"]) for c in report["checks"]]}, indent=2))
    return 0 if report["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
