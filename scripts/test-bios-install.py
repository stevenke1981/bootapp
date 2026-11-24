"""BIOS installer acceptance: `bootapp install/uninstall bios` on a generated 16 MiB MBR image.

Only image files created in the run directory are written; QEMU attaches the
image with snapshot=on. No physical disk, ESP or firmware state is touched.

Usage: python scripts/test-bios-install.py --qemu-dir DIR --nasm NASM
       [--bios-image bootapp-bios.img] [--bootapp target/release/bootapp(.exe)]
       [--out DIR] [--timeout SECONDS] [--keep]

Steps: build disk.img (one active FAT32-LBA partition at LBA 2048 whose VBR is
asm/vbr-fixture.asm) -> install dry run (must change nothing) -> install
--apply (exact layout checked) -> repeat install refused -> boot QEMU tcg
(`-machine pc`, single disk) and wait for the BootApp menu to auto-boot the
partition: `BOOTAPP_BIOS_VBR_OK DL=80 SI=0620 P=<partition entry>` -> uninstall
dry run (no change) -> uninstall --apply -> image byte-identical to the original.
"""
import argparse
import hashlib
import json
import os
import pathlib
import struct
import subprocess
import sys
import time

EXE = '.exe' if os.name == 'nt' else ''
SECTOR = 512
DISK_BYTES = 16 * 1024 * 1024
PART_START = 2048
REGION = 122 * SECTOR
BACKUP_BYTES = SECTOR + 2 * REGION  # header + original + installed LBA0..=121


def sha(data):
    return hashlib.sha256(data).hexdigest()


def build_disk(vbr):
    disk = bytearray(DISK_BYTES)
    # A recognisable "original MBR": jmp $ followed by a marker string.
    code = b'\xeb\xfe' + b'BOOTAPP-TEST-ORIGINAL-MBR'
    disk[:len(code)] = code
    disk[440:444] = struct.pack('<I', 0xB00DCAFE)
    entry = struct.pack('<B3sB3sII', 0x80, b'\xfe\xff\xff', 0x0C, b'\xfe\xff\xff',
                        PART_START, DISK_BYTES // SECTOR - PART_START)
    disk[446:462] = entry
    disk[510:512] = b'\x55\xaa'
    if len(vbr) != SECTOR or vbr[-2:] != b'\x55\xaa':
        raise SystemExit('VBR fixture must be one 55AA-signed sector')
    disk[PART_START * SECTOR:(PART_START + 1) * SECTOR] = vbr
    return bytes(disk), entry


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--qemu-dir', required=True, type=pathlib.Path)
    p.add_argument('--nasm', required=True, type=pathlib.Path)
    p.add_argument('--bios-image', type=pathlib.Path, help='bootapp-bios.img (default: build one with scripts/build-bios.py)')
    p.add_argument('--bootapp', type=pathlib.Path, help='bootapp CLI (default: <repo>/target/release/bootapp)')
    p.add_argument('--out', type=pathlib.Path, help='run directory parent (default: <repo>/out)')
    p.add_argument('--timeout', type=float, default=90, help='QEMU marker timeout in seconds')
    p.add_argument('--keep', action='store_true', help='keep generated images after a PASS')
    a = p.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    run = (a.out or root / 'out').resolve() / f'test-bios-install-{time.time_ns()}'
    run.mkdir(parents=True)
    bootapp = (a.bootapp or root / 'target' / 'release' / f'bootapp{EXE}').resolve()
    result = {'result': 'FAIL', 'run': str(run), 'steps': []}

    def step(name, **info):
        result['steps'].append({'step': name, **info})
        print(f'[ok] {name}', flush=True)

    def cli(*args, expect=0):
        command = [str(bootapp), *map(str, args)]
        done = subprocess.run(command, capture_output=True, text=True, timeout=60)
        record = {'command': command, 'exit': done.returncode, 'stdout': done.stdout, 'stderr': done.stderr}
        result.setdefault('cli', []).append(record)
        if (expect == 0) != (done.returncode == 0):
            raise RuntimeError(f'unexpected exit {done.returncode} from {command}:\n{done.stdout}\n{done.stderr}')
        return done

    images = []
    try:
        if not bootapp.is_file():
            raise RuntimeError(f'{bootapp} not found; run `cargo build --release -p bootapp-cli`')
        if a.bios_image:
            bios_image = a.bios_image.resolve()
        else:
            build_dir = run / 'bios'
            subprocess.run([sys.executable, str(root / 'scripts' / 'build-bios.py'), '--nasm', str(a.nasm),
                            '--output', str(build_dir)], check=True, timeout=300)
            bios_image = build_dir / 'bootapp-bios.img'
            images.append(bios_image)
        bios = bios_image.read_bytes()
        vbr_bin = run / 'vbr-fixture.bin'
        subprocess.run([str(a.nasm), '-f', 'bin', str(root / 'crates' / 'bootapp-bios' / 'asm' / 'vbr-fixture.asm'),
                        '-o', str(vbr_bin)], check=True, timeout=60)
        original, entry = build_disk(vbr_bin.read_bytes())
        disk = run / 'disk.img'
        disk.write_bytes(original)
        images.append(disk)
        backup = run / 'mbr-backup.bin'
        step('build', disk_sha256=sha(original), bios_image=str(bios_image), bios_sha256=sha(bios))

        out = cli('install', 'bios', '--target', disk, '--bios-image', bios_image, '--backup', backup)
        if disk.read_bytes() != original or backup.exists() or 'DRY RUN' not in out.stdout:
            raise RuntimeError('install dry run changed the image or created the backup')
        step('install dry run changed nothing')

        cli('install', 'bios', '--target', disk, '--bios-image', bios_image, '--backup', backup, '--apply')
        now = disk.read_bytes()
        saved = backup.read_bytes()
        checks = {
            'backup header': saved[:22] == b'BOOTAPP_BIOS_BACKUP 1\n' and len(saved) == BACKUP_BYTES,
            'backup original == original LBA0..=121': saved[SECTOR:SECTOR + REGION] == original[:REGION],
            'backup installed == installed LBA0..=121': saved[SECTOR + REGION:] == now[:REGION],
            'LBA0[0..440] == stage1': now[:440] == bios[:440],
            'LBA0[440..510] preserved': now[440:510] == original[440:510],
            'LBA0 signature': now[510:512] == b'\x55\xaa',
            'LBA1..=120 == stage2': now[SECTOR:121 * SECTOR] == bios[SECTOR:121 * SECTOR],
            'LBA121 == original LBA0': now[121 * SECTOR:REGION] == original[:SECTOR],
            'rest unchanged': now[REGION:] == original[REGION:],
        }
        failed = [k for k, ok in checks.items() if not ok]
        if failed:
            raise RuntimeError(f'install layout mismatch: {failed}')
        step('install --apply layout exact', installed_sha256=sha(now))

        backup2 = run / 'second-backup.bin'
        refused = cli('install', 'bios', '--target', disk, '--bios-image', bios_image, '--backup', backup2,
                      '--apply', expect=1)
        if 'already installed' not in refused.stderr or backup2.exists() or disk.read_bytes() != now:
            raise RuntimeError('second install was not refused cleanly')
        step('repeat install refused', stderr=refused.stderr.strip())

        refused = cli('install', 'bios', '--target', disk, '--bios-image', bios_image, '--backup', '--apply',
                      expect=1)
        if '--backup requires a value' not in refused.stderr or (run / '--apply').exists():
            raise RuntimeError('a flag was accepted as the --backup value')
        step('flag used as a value refused', stderr=refused.stderr.strip())

        marker = f'BOOTAPP_BIOS_VBR_OK DL=80 SI=0620 P={entry.hex().upper()}'
        serial_path = run / 'serial.log'
        command = [str(a.qemu_dir / f'qemu-system-i386{EXE}'), '-machine', 'pc', '-accel', 'tcg', '-cpu', 'pentium3,+sse2',
                   '-m', '64', '-display', 'none', '-serial', f'file:{serial_path}', '-monitor', 'none',
                   '-nic', 'none', '-no-reboot', '-drive', f'file={disk},format=raw,if=ide,index=0,snapshot=on']
        result['qemu_command'] = command
        serial = ''
        with (run / 'qemu-stderr.log').open('wb') as errors:
            proc = subprocess.Popen(command, cwd=run, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors)
            try:
                deadline = time.monotonic() + a.timeout
                while time.monotonic() < deadline:
                    serial = serial_path.read_text(errors='replace') if serial_path.exists() else ''
                    if marker in serial or 'BOOTAPP_BIOS_VBR_BAD_ABI' in serial:
                        break
                    if proc.poll() is not None:
                        raise RuntimeError(f'QEMU exited {proc.returncode}')
                    time.sleep(0.2)
            finally:
                proc.kill()
                proc.wait(timeout=10)
        result['serial'] = serial
        if marker not in serial:
            raise RuntimeError(f'marker not seen: {marker}')
        if 'BOOTAPP_BIOS_READY' not in serial:
            raise RuntimeError('BOOTAPP_BIOS_READY not seen')
        if disk.read_bytes() != now:
            raise RuntimeError('QEMU modified the image despite snapshot=on')
        step('QEMU auto-booted the partition VBR', marker=marker)

        # A boot loader installed after BootApp (GRUB-like: new LBA0 code + core.img in
        # LBA1..60) must not be destroyed by uninstall.
        foreign = bytearray(now)
        foreign[:440] = b'\x90' * 440
        foreign[SECTOR:61 * SECTOR] = b'G' * (60 * SECTOR)
        disk.write_bytes(bytes(foreign))
        refused = cli('uninstall', 'bios', '--target', disk, '--backup', backup, '--apply', expect=1)
        if disk.read_bytes() != bytes(foreign) or 'LBA 1..=60' not in refused.stderr:
            raise RuntimeError('uninstall over a foreign boot loader was not refused cleanly')
        step('uninstall refused over a later foreign boot loader', stderr=refused.stderr.strip())
        disk.write_bytes(now)

        out = cli('uninstall', 'bios', '--target', disk, '--backup', backup)
        if disk.read_bytes() != now or 'DRY RUN' not in out.stdout:
            raise RuntimeError('uninstall dry run changed the image')
        step('uninstall dry run changed nothing')
        cli('uninstall', 'bios', '--target', disk, '--backup', backup, '--apply')
        restored = disk.read_bytes()
        if restored != original:
            diff = next(i for i in range(len(original)) if restored[i] != original[i])
            raise RuntimeError(f'restored image differs from original at byte {diff}')
        step('uninstall --apply restored the original byte-for-byte', restored_sha256=sha(restored))
        result['result'] = 'PASS'
    except Exception as error:  # report every failure in result.json
        result['error'] = str(error)
    finally:
        if result['result'] == 'PASS' and not a.keep:
            for image in images:
                image.unlink(missing_ok=True)
            result['images_deleted'] = [str(i) for i in images]
        (run / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps({'result': result['result'], 'report': str(run / 'result.json'), 'error': result.get('error')}, indent=2))
    return 0 if result['result'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
