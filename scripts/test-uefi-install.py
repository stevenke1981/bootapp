"""UEFI installer acceptance: `bootapp install/uninstall uefi` into a directory, booted by OVMF.

Only the run directory is written; the ESP image is generated from it and
attached with snapshot=on; OVMF gets a PRIVATE copy of the vars template.
`--register` (NVRAM) is not exercised here: it needs Windows admin on the host.

Usage: python scripts/test-uefi-install.py --qemu-dir DIR [--out DIR]
       [--firmware-code PATH] [--firmware-vars PATH] [--bootapp PATH]
       [--no-build] [--existing-fallback] [--esp-format mbr|hdd] [--timeout S] [--keep]

Steps: empty `esp/` (optionally with a pre-existing EFI\\BOOT\\BOOTX64.EFI to
exercise backup/restore) -> install dry run (no change) -> install --apply
--fallback --config bootapp.cfg -> add payload.efi -> vvfat image -> OVMF x64
boot: BOOTAPP_MANAGER_READY and BOOTAPP_PAYLOAD_OK -> remove payload ->
uninstall dry run (no change) -> uninstall --apply -> tree equals pre-install.
"""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import time

EXE = '.exe' if os.name == 'nt' else ''


def snapshot(base):
    tree = {}
    for path in sorted(base.rglob('*')):
        rel = path.relative_to(base).as_posix()
        tree[rel] = None if path.is_dir() else path.read_bytes()
    return tree


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--qemu-dir', required=True, type=pathlib.Path)
    p.add_argument('--out', type=pathlib.Path, help='run directory parent (default: <repo>/out)')
    p.add_argument('--firmware-code', type=pathlib.Path, help='default: <qemu-dir>/share/edk2-x86_64-code.fd')
    p.add_argument('--firmware-vars', type=pathlib.Path, help='vars TEMPLATE (copied); default: <qemu-dir>/share/edk2-i386-vars.fd')
    p.add_argument('--bootapp', type=pathlib.Path, help='bootapp CLI (default: <repo>/target/release/bootapp)')
    p.add_argument('--no-build', action='store_true', help='do not run cargo build for the UEFI binaries')
    p.add_argument('--existing-fallback', action='store_true', help='pre-populate EFI\\BOOT\\BOOTX64.EFI to test backup/restore')
    p.add_argument('--esp-format', choices=['mbr', 'hdd'], default='mbr',
                   help='mbr (default): 1.44 MB vvfat FAT12 volume wrapped in an MBR type-0xEF partition (~2.5 MiB image); '
                        'hdd: QEMU vvfat hard-disk layout (504 MiB sparse file)')
    p.add_argument('--timeout', type=float, default=120)
    p.add_argument('--keep', action='store_true', help='keep esp.img / vars copy after a PASS')
    a = p.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    qemu = a.qemu_dir.resolve()
    code = (a.firmware_code or qemu / 'share' / 'edk2-x86_64-code.fd').resolve()
    vars_template = (a.firmware_vars or qemu / 'share' / 'edk2-i386-vars.fd').resolve()
    bootapp = (a.bootapp or root / 'target' / 'release' / f'bootapp{EXE}').resolve()
    run = (a.out or root / 'out').resolve() / f'test-uefi-install-{time.time_ns()}'
    esp = run / 'esp'
    esp.mkdir(parents=True)
    result = {'result': 'FAIL', 'run': str(run), 'steps': []}
    generated = []

    def step(name, **info):
        result['steps'].append({'step': name, **info})
        print(f'[ok] {name}', flush=True)

    def cli(*args, expect=0):
        command = [str(bootapp), *map(str, args)]
        done = subprocess.run(command, capture_output=True, text=True, timeout=60)
        result.setdefault('cli', []).append({'command': command, 'exit': done.returncode,
                                             'stdout': done.stdout, 'stderr': done.stderr})
        if (expect == 0) != (done.returncode == 0):
            raise RuntimeError(f'unexpected exit {done.returncode} from {command}:\n{done.stdout}\n{done.stderr}')
        return done

    try:
        if not a.no_build:
            subprocess.run(['cargo', 'build', '--locked', '--release', '--target', 'x86_64-unknown-uefi', '-p', 'bootapp-efi'],
                           cwd=root, check=True, timeout=900)
        binaries = root / 'target' / 'x86_64-unknown-uefi' / 'release'
        manager = binaries / 'bootapp-efi.efi'
        payload = binaries / 'bootapp-payload.efi'
        for needed in (bootapp, manager, payload, code, vars_template):
            if not needed.is_file():
                raise RuntimeError(f'missing {needed}')
        if a.existing_fallback:
            (esp / 'EFI' / 'BOOT').mkdir(parents=True)
            (esp / 'EFI' / 'BOOT' / 'BOOTX64.EFI').write_bytes(b'pre-existing vendor fallback loader (test data)')
            (esp / 'EFI' / 'vendor.txt').write_text('unrelated vendor file\n')
        config = run / 'bootapp.cfg'
        config.write_text('timeout=2\ndefault=payload\n[entry payload]\ntitle=Installer acceptance payload\n'
                          'path=\\EFI\\BOOTAPP\\payload.efi\narch=x64\noptions=bootapp-test\n', encoding='utf-8')
        before = snapshot(esp)
        install = ['install', 'uefi', '--esp', esp, '--arch', 'x64', '--manager', manager,
                   '--config', config, '--fallback']
        out = cli(*install)
        if snapshot(esp) != before or 'DRY RUN' not in out.stdout:
            raise RuntimeError('install dry run changed the ESP directory')
        step('install dry run changed nothing')
        cli(*install, '--apply')
        bootapp_dir = esp / 'EFI' / 'BOOTAPP'
        expected = {
            'EFI/BOOTAPP/BOOTAPPX64.EFI': manager.read_bytes(),
            'EFI/BOOT/BOOTX64.EFI': manager.read_bytes(),
            'EFI/BOOTAPP/bootapp.cfg': config.read_bytes(),
        }
        if a.existing_fallback:
            expected['EFI/BOOTAPP/backup/BOOTX64.EFI'] = before['EFI/BOOT/BOOTX64.EFI']
        tree = snapshot(esp)
        wrong = [k for k, v in expected.items() if tree.get(k) != v]
        manifest = (bootapp_dir / 'install-manifest.txt').read_text(encoding='utf-8')
        if wrong or 'state=complete' not in manifest:
            raise RuntimeError(f'install result mismatch: {wrong}\n{manifest}')
        step('install --apply wrote manager, fallback, config, manifest', manifest=manifest)
        again = cli(*install, '--apply')
        if snapshot(esp) != tree or 'idempotent' not in again.stdout:
            raise RuntimeError('identical re-install was not a no-op')
        step('identical re-install is a no-op')

        shutil.copyfile(payload, bootapp_dir / 'payload.efi')
        image = run / 'esp.img'
        if a.esp_format == 'hdd':
            subprocess.run([str(qemu / f'qemu-img{EXE}'), 'convert', '-f', 'vvfat', 'fat:esp', '-O', 'raw', 'esp.img'],
                           cwd=run, check=True, timeout=60)
            generated.append(image)
        else:
            # OVMF BDS does not boot an unpartitioned (superfloppy) disk, so wrap the
            # small vvfat FAT12 volume in an MBR partition of type 0xEF at LBA 2048.
            # (FAT12, not FAT16: a 2.88 MB "FAT16" floppy has <4085 clusters and EDK2
            # correctly classifies it as FAT12 by cluster count, misreading the tables.)
            volume = run / 'esp-volume.img'
            subprocess.run([str(qemu / f'qemu-img{EXE}'), 'convert', '--image-opts',
                            'driver=vvfat,dir=esp,floppy=on,fat-type=12', '-O', 'raw', volume.name],
                           cwd=run, check=True, timeout=60)
            generated.append(volume)
            fat = bytearray(volume.read_bytes())
            sectors = len(fat) // 512
            fat[0x1c:0x20] = (2048).to_bytes(4, 'little')  # BPB hidden sectors = partition start
            mbr = bytearray(512)
            mbr[440:444] = (0xE5B007A9).to_bytes(4, 'little')
            mbr[446:462] = (bytes([0x00, 0xfe, 0xff, 0xff, 0xef, 0xfe, 0xff, 0xff])
                            + (2048).to_bytes(4, 'little') + sectors.to_bytes(4, 'little'))
            mbr[510:512] = bytes([0x55, 0xAA])
            image.write_bytes(bytes(mbr) + bytes(2047 * 512) + bytes(fat))
            generated.append(image)
        vars_copy = run / 'vars.fd'
        shutil.copyfile(vars_template, vars_copy)
        generated.append(vars_copy)
        serial_path = run / 'serial.log'
        command = [str(qemu / f'qemu-system-x86_64{EXE}'), '-machine', 'q35', '-accel', 'tcg', '-m', '256',
                   '-display', 'none', '-serial', f'file:{serial_path}', '-monitor', 'none', '-nic', 'none', '-no-reboot',
                   '-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={code}',
                   '-drive', f'if=pflash,format=raw,unit=1,file={vars_copy}',
                   '-drive', f'file={image},format=raw,if=ide,snapshot=on']
        result['qemu_command'] = command
        serial = ''
        markers = ('BOOTAPP_MANAGER_READY', 'BOOTAPP_PAYLOAD_OPTIONS bootapp-test', 'BOOTAPP_PAYLOAD_OK')
        with (run / 'qemu-stderr.log').open('wb') as errors:
            proc = subprocess.Popen(command, cwd=run, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors)
            try:
                deadline = time.monotonic() + a.timeout
                while time.monotonic() < deadline:
                    serial = serial_path.read_text(encoding='utf-8', errors='replace') if serial_path.exists() else ''
                    if all(m in serial for m in markers):
                        break
                    if proc.poll() is not None:
                        break
                    time.sleep(0.25)
            finally:
                proc.kill()
                proc.wait(timeout=10)
        result['serial'] = serial
        missing = [m for m in markers if m not in serial]
        if missing:
            raise RuntimeError(f'serial markers missing: {missing}')
        step('OVMF booted the installed fallback into BootApp and the payload', markers=list(markers))

        (bootapp_dir / 'payload.efi').unlink()

        # A tampered manifest naming a foreign file must be rejected as a whole.
        manifest_path = bootapp_dir / 'install-manifest.txt'
        genuine = manifest_path.read_bytes()
        foreign = esp / 'EFI' / 'Microsoft' / 'Boot' / 'bootmgfw.efi'
        foreign.parent.mkdir(parents=True)
        foreign.write_bytes(b'foreign boot manager (test data)')
        digest = hashlib.sha256(foreign.read_bytes()).hexdigest()
        manifest_path.write_bytes(genuine + (f'file=EFI\\Microsoft\\Boot\\bootmgfw.efi|'
                                             f'{foreign.stat().st_size}|{digest}\n').encode())
        tampered = snapshot(esp)
        refused = cli('uninstall', 'uefi', '--esp', esp, '--apply', expect=1)
        if snapshot(esp) != tampered or 'non-BootApp entry' not in refused.stderr:
            raise RuntimeError(f'tampered manifest was not rejected cleanly: {refused.stderr}')
        step('tampered manifest rejected; foreign file untouched', stderr=refused.stderr.strip())
        manifest_path.write_bytes(genuine)
        shutil.rmtree(esp / 'EFI' / 'Microsoft')

        installed = snapshot(esp)
        out = cli('uninstall', 'uefi', '--esp', esp)
        if snapshot(esp) != installed or 'DRY RUN' not in out.stdout:
            raise RuntimeError('uninstall dry run changed the ESP directory')
        step('uninstall dry run changed nothing')
        cli('uninstall', 'uefi', '--esp', esp, '--apply')
        after = snapshot(esp)
        if after != before:
            raise RuntimeError(f'ESP not restored: before={sorted(before)} after={sorted(after)}')
        step('uninstall --apply restored the pre-install tree exactly', entries=sorted(after))
        result['result'] = 'PASS'
    except Exception as error:  # report every failure in result.json
        result['error'] = str(error)
    finally:
        if result['result'] == 'PASS' and not a.keep:
            for path in generated:
                path.unlink(missing_ok=True)
            result['images_deleted'] = [str(p) for p in generated]
        (run / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps({'result': result['result'], 'report': str(run / 'result.json'), 'error': result.get('error')}, indent=2))
    return 0 if result['result'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
