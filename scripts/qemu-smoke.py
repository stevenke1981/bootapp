"""Boot isolated snapshot directory media; no physical disks or host boot changes.

Usage: python scripts/qemu-smoke.py --qemu-dir <directory> [--arch x64|x86|aarch64]
       [--case auto|missing|self|keyboard|invalid] [--out DIR]
       [--firmware-code PATH] [--firmware-vars PATH]

Each run creates `<out>/vm-<arch>-<case>-<ns>/` (default `<repo>/out`). A
`--firmware-vars` template is COPIED into the run directory and only that
private copy is attached writable; the template itself is never modified.
"""
import argparse
import json
import os
import pathlib
import shutil
import socket
import subprocess
import time

EXE = '.exe' if os.name == 'nt' else ''


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--qemu-dir', type=pathlib.Path, default=os.environ.get('BOOTAPP_QEMU_DIR'),
                        required='BOOTAPP_QEMU_DIR' not in os.environ)
    parser.add_argument('--arch', choices=['x64', 'x86', 'aarch64'], default='x64')
    parser.add_argument('--case', choices=['auto', 'missing', 'self', 'keyboard', 'invalid'], default='auto')
    parser.add_argument('--out', type=pathlib.Path, help='run directory parent (default: <repo>/out)')
    parser.add_argument('--firmware-code', type=pathlib.Path, help='UEFI code image (default: <qemu-dir>/share/edk2-<arch>-code.fd)')
    parser.add_argument('--firmware-vars', type=pathlib.Path, help='optional vars TEMPLATE; a private copy is attached')
    parser.add_argument('--timeout', type=float, default=60)
    parser.add_argument('--keep-images', action='store_true', help='keep the generated esp.img / vars.fd')
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    qemu_dir = pathlib.Path(args.qemu_dir).resolve()
    target, filename, emulator, firmware = {
        'x64': ('x86_64-unknown-uefi', 'BOOTX64.EFI', 'x86_64', 'x86_64'),
        'x86': ('i686-unknown-uefi', 'BOOTIA32.EFI', 'i386', 'i386'),
        'aarch64': ('aarch64-unknown-uefi', 'BOOTAA64.EFI', 'aarch64', 'aarch64'),
    }[args.arch]
    code = (args.firmware_code or qemu_dir / 'share' / f'edk2-{firmware}-code.fd').resolve()
    binaries = root / 'target' / target / 'release'
    for required in (code, qemu_dir / f'qemu-img{EXE}', qemu_dir / f'qemu-system-{emulator}{EXE}',
                     binaries / 'bootapp-efi.efi', binaries / 'bootapp-payload.efi'):
        if not required.is_file():
            parser.error(f'Required regular file is missing: {required}')
    test = (args.out or root / 'out').resolve() / f'vm-{args.arch}-{args.case}-{time.time_ns()}'
    esp = test / 'esp'
    (esp / 'EFI' / 'BOOT').mkdir(parents=True)
    (esp / 'EFI' / 'BOOTAPP').mkdir()
    shutil.copyfile(binaries / 'bootapp-efi.efi', esp / 'EFI' / 'BOOT' / filename)
    shutil.copyfile(binaries / 'bootapp-payload.efi', esp / 'EFI' / 'BOOTAPP' / 'payload.efi')
    path = {'missing': r'\EFI\BOOTAPP\missing.efi', 'self': '\\EFI\\BOOT\\' + filename}.get(args.case, r'\EFI\BOOTAPP\payload.efi')
    config = f'timeout=2\ndefault=test\n[entry test]\ntitle=EFI acceptance payload\npath={path}\narch={args.arch}\noptions=bootapp-test\n'
    if args.case == 'invalid':
        config += 'unexpected=value\n'
    (esp / 'EFI' / 'BOOTAPP' / 'bootapp.cfg').write_text(config, encoding='utf-8')
    vars_copy = None
    if args.firmware_vars:
        vars_copy = test / 'vars.fd'
        shutil.copyfile(args.firmware_vars, vars_copy)
    # Materialize vvfat once: current QEMU rejects writable vvfat underneath a
    # read-only snapshot base. Only this generated file is attached to the guest.
    subprocess.run([str(qemu_dir / f'qemu-img{EXE}'), 'convert', '-f', 'vvfat',
                    'fat:esp', '-O', 'raw', 'esp.img'], cwd=test, check=True, timeout=30)
    shutil.rmtree(esp)
    probe = socket.socket()
    probe.bind(('127.0.0.1', 0))
    port = probe.getsockname()[1]
    probe.close()
    command = [str(qemu_dir / f'qemu-system-{emulator}{EXE}'), '-accel', 'tcg', '-m', '256',
               '-display', 'none', '-serial', 'file:serial.log', '-monitor', 'none', '-nic', 'none',
               '-no-reboot', '-qmp', f'tcp:127.0.0.1:{port},server=on,wait=off']
    if args.arch == 'aarch64':
        command += ['-machine', 'virt', '-cpu', 'cortex-a72']
        if vars_copy:
            command += ['-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={code}',
                        '-drive', f'if=pflash,format=raw,unit=1,file={vars_copy}']
        else:
            command += ['-bios', str(code)]
        command += ['-drive', 'file=esp.img,format=raw,if=none,id=esp,snapshot=on', '-device', 'virtio-blk-device,drive=esp']
    else:
        command += ['-machine', 'q35', '-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={code}']
        if vars_copy:
            command += ['-drive', f'if=pflash,format=raw,unit=1,file={vars_copy}']
        command += ['-drive', 'file=esp.img,format=raw,if=ide,snapshot=on']
    result = {'architecture': args.arch, 'case': args.case, 'command': command, 'cwd': str(test), 'result': 'FAIL'}
    connection = None
    wire = None
    serial = ''

    def qmp(name, arguments=None):
        wire.write((json.dumps({'execute': name, 'arguments': arguments or {}}) + '\n').encode())
        wire.flush()
        while True:
            line = wire.readline()
            if not line:
                raise RuntimeError('QMP closed unexpectedly')
            data = json.loads(line)
            if 'error' in data:
                raise RuntimeError(data)
            if 'return' in data:
                return data['return']
    with (test / 'stderr.log').open('wb') as errors:
        process = subprocess.Popen(command, cwd=test, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f'QEMU exited {process.returncode}: {(test / "stderr.log").read_text()}')
                try:
                    connection = socket.create_connection(('127.0.0.1', port), timeout=1)
                    break
                except OSError:
                    time.sleep(0.1)
            if connection is None:
                raise RuntimeError('QMP startup timeout')
            connection.settimeout(5)
            wire = connection.makefile('rwb')
            json.loads(wire.readline())
            qmp('qmp_capabilities')
            sent = False
            entered = False
            ready_at = None
            expected = {'auto': 'BOOTAPP_PAYLOAD_OK', 'missing': 'Boot failed: NOT_FOUND', 'self': 'Boot failed: ACCESS_DENIED',
                        'invalid': 'BOOTAPP_CONFIG_ERROR', 'keyboard': 'BOOTAPP_PAYLOAD_OK'}[args.case]
            while time.monotonic() < deadline:
                serial = (test / 'serial.log').read_text(encoding='utf-8', errors='replace') if (test / 'serial.log').exists() else ''
                if 'BOOTAPP_MANAGER_READY' in serial and ready_at is None:
                    ready_at = time.monotonic()
                if args.case == 'keyboard' and ready_at is not None:
                    if not sent:
                        qmp('send-key', {'keys': [{'type': 'qcode', 'data': 'esc'}]})
                        sent = True
                    if time.monotonic() - ready_at > 5 and not entered:
                        if expected in serial:
                            raise RuntimeError('key did not cancel automatic boot')
                        result['timeout_cancelled'] = True
                        qmp('send-key', {'keys': [{'type': 'qcode', 'data': 'ret'}]})
                        entered = True
                if expected in serial and (args.case != 'keyboard' or entered):
                    if args.case in ('auto', 'keyboard') and 'BOOTAPP_CHILD_RETURN Ok(())' not in serial:
                        time.sleep(0.1)
                        continue
                    if args.case in ('auto', 'keyboard') and 'BOOTAPP_PAYLOAD_OPTIONS bootapp-test' not in serial:
                        raise RuntimeError('payload did not report the configured load options')
                    result['result'] = 'PASS'
                    result['expected_marker'] = expected
                    result['serial'] = serial
                    if args.arch != 'aarch64':
                        qmp('screendump', {'filename': str(test / 'screen.ppm')})
                    break
                time.sleep(0.15)
            else:
                raise RuntimeError(f'timeout waiting for {expected}')
        except Exception as error:
            result['error'] = str(error)
            result['serial'] = serial
        finally:
            if wire:
                try:
                    qmp('quit')
                except (OSError, ValueError, RuntimeError):
                    pass
                wire.close()
            if connection:
                connection.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
            result['process_exit_code'] = process.returncode
            if not args.keep_images:
                for image in (test / 'esp.img', vars_copy):
                    if image and image.exists():
                        image.unlink()
    report = test / 'result.json'
    report.write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps({'result': result['result'], 'report': str(report), 'error': result.get('error')}, indent=2))
    return 0 if result['result'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
