"""BIOS fixture acceptance; no real-OS claim. Uses isolated snapshot images.

Cases
  auto           BootApp image (IDE0) + fixture MBR disk (IDE1); default entry
                 auto-boots after the live 5..1 countdown -> FIXTURE_OK DL=81.
  keyboard       as auto, Esc cancels the countdown, Enter after 7 s boots it.
  empty          BootApp image only: no entry, no chainload.
  own-partition  one 16 MiB disk laid out exactly like the installer output
                 (BRIEF install layout): LBA0 = stage1 code + partition table
                 (one active type-0x07 partition at LBA 2048), LBA1..120 =
                 stage2, LBA121 = signed "original MBR" (fixture -DSAVED_MBR +
                 same table), LBA2048 = vbr-fixture. Default entry order is
                 own partitions 1-4, then saved original MBR, then other disks,
                 so auto-boot chainloads partition 1 -> VBR_OK DL=80 SI=0620 P=..
  saved-mbr      same disk, but LBA121 carries a stale install-time table
                 (partition 1 size differs); Down + Enter selects "Saved
                 original MBR" -> SAVED_MBR_OK DL=80 P=<entry 1 the MBR sees>,
                 which must be the CURRENT LBA0 entry (table patched in 0x8000).
  saved-mbr-blank  same disk with LBA121 code bytes 0..440 all zero: the saved
                 MBR must not be listed; partition 1 auto-boots.
  no-sse2        BootApp image on -cpu pentium3 (no SSE2): stage2 must refuse
                 with BOOTAPP_BIOS_CPU_UNSUPPORTED instead of faulting.

TCG runs use -cpu pentium3,+sse2 (QEMU 11 has no pentium4 model; this is the
minimal FXSR+SSE+SSE2 CPU, i.e. the Pentium 4 baseline). The prebuilt i686 Rust core emits SSE2, and TCG
does not fault on SSE2 opcodes even when CPUID lacks them, so a pentium3 run
would not be valid evidence.
"""
import argparse
import json
import os
import pathlib
import socket
import subprocess
import time

EXE = '.exe' if os.name == 'nt' else ''
SECTOR = 512
DISK_BYTES = 16 * 1024 * 1024
PARTITION_LBA = 2048
SAVED_MBR_LBA = 121
CASES = ['auto', 'keyboard', 'empty', 'own-partition', 'saved-mbr', 'saved-mbr-blank', 'no-sse2']
OWN_DISK_CASES = ('own-partition', 'saved-mbr', 'saved-mbr-blank')


def assemble(nasm, source, output, *defines):
    subprocess.run([str(nasm), '-f', 'bin', *defines, str(source), '-o', str(output)], check=True)
    data = output.read_bytes()
    if len(data) != SECTOR or data[510:] != b'\x55\xaa':
        raise RuntimeError(f'{source.name} is not one signed sector')
    return data


def partition_table(size=DISK_BYTES // SECTOR - PARTITION_LBA):
    """Disk signature + one active NTFS/IFS (0x07) partition at LBA 2048."""
    # CHS for LBA 2048 with 255 heads / 63 sectors: C=0 H=32 S=33; end saturated.
    entry = bytes([0x80, 0x20, 0x21, 0x00, 0x07, 0xFE, 0xFF, 0xFF]) + \
        PARTITION_LBA.to_bytes(4, 'little') + size.to_bytes(4, 'little')
    table = (0x33415042).to_bytes(4, 'little') + bytes(2) + entry + bytes(48)
    assert len(table) == 70
    return table, entry


def build_own_disk(image_path, run, nasm, asm, case):
    """Return (disk path, current 16-byte partition entry, stale entry or None)."""
    image = image_path.read_bytes()
    if len(image) < (SAVED_MBR_LBA) * SECTOR or image[510:512] != b'\x55\xaa':
        raise RuntimeError('BootApp image too small or LBA0 unsigned')
    table, entry = partition_table()
    vbr = assemble(nasm, asm / 'vbr-fixture.asm', run / 'vbr-fixture.bin')
    original = assemble(nasm, asm / 'fixture.asm', run / 'saved-mbr.bin', '-DSAVED_MBR')
    if any(original[440:510]):
        raise RuntimeError('fixture.asm -DSAVED_MBR overlaps the partition table area')
    stale_entry = None
    saved_table = table
    if case == 'saved-mbr':
        # Partition 1 was resized after installation: LBA121 keeps the old table.
        saved_table, stale_entry = partition_table(size=0x2800)
    if case == 'saved-mbr-blank':
        original = bytes(440) + original[440:]                  # table, no boot code
    original = original[:440] + saved_table + b'\x55\xaa'     # the target's original LBA0
    mbr = image[:440] + table + b'\x55\xaa'                   # installed LBA0 (current table)
    disk = bytearray(DISK_BYTES)
    disk[0:SECTOR] = mbr
    disk[SECTOR:SAVED_MBR_LBA * SECTOR] = image[SECTOR:SAVED_MBR_LBA * SECTOR]  # LBA1..=120
    disk[SAVED_MBR_LBA * SECTOR:(SAVED_MBR_LBA + 1) * SECTOR] = original
    disk[PARTITION_LBA * SECTOR:(PARTITION_LBA + 1) * SECTOR] = vbr
    assert mbr[:440] != original[:440]
    path = run / 'own-disk.img'
    path.write_bytes(disk)
    return path, entry, stale_entry


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    p.add_argument('--qemu-dir', required=True, type=pathlib.Path)
    p.add_argument('--nasm', required=True, type=pathlib.Path)
    p.add_argument('--image', required=True, type=pathlib.Path)
    p.add_argument('--case', choices=CASES, default='auto')
    p.add_argument('--accel', choices=['tcg', 'whpx', 'kvm'], default='tcg',
                   help='whpx/kvm enforce real-mode segment limits that TCG ignores')
    p.add_argument('--out', type=pathlib.Path, help='run directory parent (default: <repo>/out)')
    a = p.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    asm = root / 'crates' / 'bootapp-bios' / 'asm'
    run = (a.out or root / 'out') / f'test-{a.case}-{a.accel}-{time.time_ns()}'
    run.mkdir(parents=True)
    own_disk = a.case in OWN_DISK_CASES
    expected_entry = stale_entry = None
    if own_disk:
        boot_disk, expected_entry, stale_entry = build_own_disk(a.image.resolve(), run, a.nasm, asm, a.case)
    else:
        boot_disk = a.image.resolve()
        fixture = run / 'fixture.img'
        assemble(a.nasm, asm / 'fixture.asm', fixture)
        with fixture.open('ab') as f:
            f.write(bytes(1024 * 1024 - SECTOR))
    sock = socket.socket()
    sock.bind(('127.0.0.1', 0))
    port = sock.getsockname()[1]
    sock.close()
    emulator = ('qemu-system-i386' if a.accel == 'tcg' else 'qemu-system-x86_64') + EXE
    if a.case == 'no-sse2':
        cpu = ['-cpu', 'pentium3']          # FXSR+SSE, no SSE2
    else:
        cpu = ['-cpu', 'pentium3,+sse2'] if a.accel == 'tcg' else ['-cpu', 'host']
    cmd = [str(a.qemu_dir / emulator), '-machine', 'pc', '-accel', a.accel, *cpu, '-m', '64',
           '-display', 'none', '-serial', 'file:serial.log', '-monitor', 'none', '-nic', 'none', '-no-reboot',
           '-qmp', f'tcp:127.0.0.1:{port},server=on,wait=off',
           '-drive', f'file={boot_disk},format=raw,if=ide,index=0,snapshot=on']
    if a.case in ('auto', 'keyboard'):
        cmd += ['-drive', f'file={fixture},format=raw,if=ide,index=1,snapshot=on']
    if a.case in ('auto', 'keyboard'):
        marker = 'BOOTAPP_BIOS_FIXTURE_OK DL=81 REALMODE=1 SEGMENTS=0'
    elif a.case in ('own-partition', 'saved-mbr-blank'):
        marker = 'BOOTAPP_BIOS_VBR_OK DL=80 SI=0620 P=' + expected_entry.hex().upper()
    elif a.case == 'saved-mbr':
        marker = 'BOOTAPP_BIOS_SAVED_MBR_OK DL=80 REALMODE=1 SEGMENTS=0 P=' + expected_entry.hex().upper()
    elif a.case == 'no-sse2':
        marker = 'BOOTAPP_BIOS_CPU_UNSUPPORTED'
    else:
        marker = None
    result = {'case': a.case, 'accel': a.accel, 'command': cmd, 'expected_marker': marker, 'result': 'FAIL'}
    conn = None
    wire = None
    serial = ''

    def qmp(command, arguments=None):
        wire.write((json.dumps({'execute': command, 'arguments': arguments or {}}) + '\n').encode())
        wire.flush()
        while True:
            response = json.loads(wire.readline())
            if 'error' in response:
                raise RuntimeError(response)
            if 'return' in response:
                return response['return']

    def key(name):
        qmp('send-key', {'keys': [{'type': 'qcode', 'data': name}]})

    def check_final(serial):
        lines = serial.splitlines()
        if a.case == 'no-sse2':
            assert 'BOOTAPP_BIOS_READY' not in serial, 'Rust must not run without SSE2'
            return
        # SeaBIOS EBDA is at 0x9FC00, so min(0x90000, EBDA) keeps the ceiling.
        assert 'BOOTAPP_BIOS_STACK top=00090000' in serial, 'stack top trace'
        if a.case == 'auto':
            countdown = [line.split()[-1] for line in lines if line.startswith('BOOTAPP_BIOS_COUNTDOWN ')]
            assert countdown == ['5', '4', '3', '2', '1'], f'countdown trace {countdown}'
            assert 'BOOTAPP_BIOS_KEY' not in serial
        if own_disk:
            assert 'BOOTAPP_BIOS_DETECTED drive=00000080 partition=1 lba=00000800 kind=Unknown' in serial
            assert 'lba=00000000' not in serial, 'own LBA0 must never be offered'
        if a.case in ('own-partition', 'saved-mbr'):
            assert 'BOOTAPP_BIOS_DETECTED drive=00000080 saved_mbr lba=00000079 kind=Unknown' in serial
            assert serial.count('BOOTAPP_BIOS_DETECTED') == 2, 'unexpected extra entries'
        if a.case == 'saved-mbr-blank':
            assert 'saved_mbr' not in serial, 'blank saved MBR must not be listed'
            assert serial.count('BOOTAPP_BIOS_DETECTED') == 1, 'unexpected extra entries'
            assert 'BOOTAPP_BIOS_CHAINLOAD drive=00000080 lba=00000800' in serial
        if a.case == 'own-partition':
            assert 'BOOTAPP_BIOS_CHAINLOAD drive=00000080 lba=00000800' in serial
            assert 'SAVED_MBR_OK' not in serial
        if a.case == 'saved-mbr':
            assert 'BOOTAPP_BIOS_CHAINLOAD drive=00000080 lba=00000079 table=current' in serial
            assert 'VBR_OK' not in serial
            assert stale_entry.hex().upper() not in serial, 'stale saved table was booted'

    with (run / 'stderr.log').open('wb') as errors:
        proc = subprocess.Popen(cmd, cwd=run, stdout=subprocess.DEVNULL, stderr=errors)
        try:
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                try:
                    conn = socket.create_connection(('127.0.0.1', port), timeout=1)
                    break
                except OSError:
                    time.sleep(.1)
            if conn is None:
                raise RuntimeError('QMP connection timeout')
            conn.settimeout(3)
            wire = conn.makefile('rwb')
            wire.readline()
            qmp('qmp_capabilities')
            ready = None
            sent = False
            entered = False
            while time.monotonic() < deadline:
                serial = (run / 'serial.log').read_text(errors='replace') if (run / 'serial.log').exists() else ''
                if a.case == 'no-sse2' and 'BOOTAPP_BIOS_READY' in serial:
                    raise RuntimeError('Rust manager ran on a CPU without SSE2')
                if 'BOOTAPP_BIOS_READY' in serial and ready is None:
                    ready = time.monotonic()
                if a.case == 'empty' and ready and time.monotonic() - ready > 2:
                    assert 'DETECTED' not in serial and 'CHAINLOAD' not in serial
                    result['result'] = 'PASS'
                    break
                if a.case == 'keyboard' and ready:
                    if not sent:
                        key('esc')
                        sent = True
                    if time.monotonic() - ready > 7 and not entered:
                        assert 'FIXTURE_OK' not in serial, 'timeout was not cancelled'
                        key('ret')
                        entered = True
                if a.case == 'saved-mbr' and 'BOOTAPP_BIOS_COUNTDOWN 5' in serial:
                    if not sent:
                        key('down')
                        sent = True
                    elif not entered and 'BOOTAPP_BIOS_KEY' in serial:
                        key('ret')
                        entered = True
                if marker and marker in serial:
                    check_final(serial)
                    result['result'] = 'PASS'
                    break
                for bad in ('BAD_ABI', 'BOOTAPP_BIOS_PANIC', 'CHAINLOAD_ERROR'):
                    if bad in serial:
                        raise RuntimeError(f'{bad} in serial log')
                if proc.poll() is not None:
                    raise RuntimeError(f'QEMU exited {proc.returncode}')
                time.sleep(.1)
            else:
                raise RuntimeError('fixture marker timeout')
            qmp('screendump', {'filename': str(run / 'screen.ppm')})
        except Exception as e:
            result['error'] = f'{type(e).__name__}: {e}'
        finally:
            result['serial'] = serial
            if wire:
                try:
                    qmp('quit')
                except Exception:
                    pass
                wire.close()
            if conn:
                conn.close()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.terminate()
                proc.wait(timeout=3)
            result['qemu_exit_code'] = proc.returncode
    (run / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps({'result': result['result'], 'report': str(run / 'result.json'), 'error': result.get('error')}))
    return 0 if result['result'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
