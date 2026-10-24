"""Start/control isolated actual-OS boot labs. Images are never attached writable.

start creates a project-owned qcow2 overlay, and records the exact QEMU argv.
It does not assert OS boot success; guest evidence must be collected separately.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import socket
import struct
import subprocess
import time
import zlib


def qmp(port, command, arguments=None):
    with socket.create_connection(('127.0.0.1', port), timeout=5) as connection:
        connection.settimeout(10)
        with connection.makefile('rwb') as wire:
            json.loads(wire.readline())
            for name, args in [('qmp_capabilities', {}), (command, arguments or {})]:
                wire.write((json.dumps({'execute': name, 'arguments': args}) + '\n').encode())
                wire.flush()
                while True:
                    obj = json.loads(wire.readline())
                    if 'error' in obj:
                        raise RuntimeError(obj)
                    if 'return' in obj:
                        result = obj['return']
                        break
    return result


def png_from_ppm(source, destination):
    import re
    data = source.read_bytes()
    match = re.match(rb'P6\s+(\d+)\s+(\d+)\s+255\s', data)
    if not match:
        raise ValueError('Unsupported QEMU screenshot format')
    width, height = map(int, match.groups())
    rgb = data[match.end():]
    if len(rgb) != width * height * 3:
        raise ValueError('Truncated screenshot')
    def chunk(tag, payload):
        return struct.pack('>I', len(payload)) + tag + payload + struct.pack('>I', zlib.crc32(tag + payload) & 0xffffffff)
    scan = b''.join(b'\0' + rgb[i*width*3:(i+1)*width*3] for i in range(height))
    destination.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(scan)) + chunk(b'IEND', b''))


def start(a):
    root = pathlib.Path(__file__).resolve().parents[1]
    disk = a.disk.resolve(strict=True)
    if not disk.is_file() or str(disk).startswith('\\\\.\\'):
        raise ValueError('Only regular source image files are accepted')
    if a.output.exists():
        raise ValueError('Lab output must be a new directory')
    # Verify both listening ports before any process launch.
    for port in [a.qmp_port, a.ssh_port]:
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', port))
    a.output.mkdir(parents=True)
    output = a.output.resolve()
    image_tool = a.qemu_dir / 'qemu-img.exe'
    info = json.loads(subprocess.check_output([str(image_tool), 'info', '--output=json', str(disk)], text=True))
    subprocess.run([str(image_tool), 'create', '-f', 'qcow2', '-F', info['format'], '-b', str(disk), str(output / 'os.qcow2')], check=True)
    command = [str(a.qemu_dir / 'qemu-system-x86_64.exe'), '-name', f'bootapp-{a.os}-{a.mode}', '-machine', 'q35',
               '-accel', 'tcg,thread=multi', '-cpu', 'max', '-smp', '2', '-m', str(a.memory), '-display', 'none',
               '-monitor', 'none', '-serial', 'file:serial.log', '-qmp', f'tcp:127.0.0.1:{a.qmp_port},server=on,wait=off',
               '-netdev', f'user,id=lab,hostfwd=tcp:127.0.0.1:{a.ssh_port}-:22',
               '-device', 'e1000,netdev=lab,mac=52:54:00:ba:02:01']
    if a.usb_input:
        command += ['-device', 'qemu-xhci,id=lab-xhci',
                    '-device', 'usb-kbd,bus=lab-xhci.0', '-device', 'usb-tablet,bus=lab-xhci.0']
    if a.mode == 'bios':
        manager = a.bios_image.resolve() if a.bios_image else root / 'out' / 'bios/bootapp-bios.img'
        shutil.copyfile(manager, output / 'manager.img')
        manager_hash = hashlib.sha256(manager.read_bytes()).hexdigest()
    else:
        manager = root / 'target/x86_64-unknown-uefi/release/bootapp-efi.efi'
        esp = output / 'esp/EFI/BOOT'
        esp.mkdir(parents=True)
        shutil.copyfile(manager, esp / 'BOOTX64.EFI')
        subprocess.run([str(image_tool), 'convert', '-f', 'vvfat', 'fat:esp', '-O', 'raw', 'manager.img'], cwd=output, check=True)
        command += ['-drive', f'if=pflash,format=raw,readonly=on,file={a.qemu_dir / "share/edk2-x86_64-code.fd"}']
        if a.persistent_vars:
            # Private guest NVRAM, never the host firmware or original template.
            shutil.copyfile(a.qemu_dir / 'share/edk2-i386-vars.fd', output / 'vars.fd')
            command += ['-drive', 'if=pflash,unit=1,format=raw,file=vars.fd']
        manager_hash = hashlib.sha256(manager.read_bytes()).hexdigest()
    command += ['-drive', 'file=manager.img,format=raw,if=ide,index=0,snapshot=on',
                '-drive', 'file=os.qcow2,format=qcow2,if=ide,index=1']
    if a.seed:
        shutil.copyfile(a.seed, output / 'seed.iso')
        command += ['-drive', 'file=seed.iso,format=raw,media=cdrom,readonly=on']
    with (output / 'stderr.log').open('wb') as errors:
        process = subprocess.Popen(command, cwd=output, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors)
    state = {'pid': process.pid, 'command': command, 'cwd': str(output), 'qmp_port': a.qmp_port, 'ssh_port': a.ssh_port,
             'source_disk': str(disk), 'source_format': info['format'], 'manager_sha256': manager_hash,
             'os': a.os, 'mode': a.mode, 'result': 'NOT_RUN', 'note': 'Launched only; requires actual guest OS evidence'}
    (output / 'lab.json').write_text(json.dumps(state, indent=2), encoding='utf-8')
    time.sleep(0.5)
    if process.poll() is not None:
        raise RuntimeError((output / 'stderr.log').read_text())
    print(json.dumps(state, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest='action', required=True)
    p = subs.add_parser('start')
    p.add_argument('--qemu-dir', type=pathlib.Path, required=True)
    p.add_argument('--disk', type=pathlib.Path, required=True)
    p.add_argument('--output', type=pathlib.Path, required=True)
    p.add_argument('--os', choices=['linux', 'windows'], required=True)
    p.add_argument('--mode', choices=['bios', 'uefi'], required=True)
    p.add_argument('--memory', type=int, default=2048)
    p.add_argument('--qmp-port', type=int, required=True)
    p.add_argument('--ssh-port', type=int, required=True)
    p.add_argument('--seed', type=pathlib.Path)
    p.add_argument('--bios-image', type=pathlib.Path)
    p.add_argument('--usb-input', action='store_true')
    p.add_argument('--persistent-vars', action='store_true')
    for action in ['status', 'screenshot', 'key', 'text', 'powerdown', 'quit']:
        p = subs.add_parser(action)
        p.add_argument('--port', type=int, required=True)
        if action == 'screenshot':
            p.add_argument('--output', type=pathlib.Path, required=True)
        if action in ('key', 'text'):
            p.add_argument('value')
    a = parser.parse_args()
    if a.action == 'start':
        start(a)
    elif a.action == 'screenshot':
        dest = a.output.resolve()
        qmp(a.port, 'screendump', {'filename': str(dest.with_suffix('.ppm'))})
        png_from_ppm(dest.with_suffix('.ppm'), dest)
        print(dest)
    elif a.action == 'key':
        print(qmp(a.port, 'send-key', {'keys': [{'type': 'qcode', 'data': a.value}]}))
    elif a.action == 'text':
        # QEMU HMP sendkey only accepts key names; text is passed as typed events,
        # never interpreted as shell/QMP command text.
        for char in a.value:
            if char.isascii() and char.isalpha():
                keys = ([{'type': 'qcode', 'data': 'shift'}] if char.isupper() else []) + [{'type': 'qcode', 'data': char.lower()}]
            elif char.isdigit():
                keys = [{'type': 'qcode', 'data': char}]
            else:
                name = {' ': 'spc', '\n': 'ret', '-': 'minus', '.': 'dot', '/': 'slash', '\\': 'backslash', ',': 'comma', ':': 'semicolon', '_': 'minus', '=': 'equal', '>': 'dot', '|': 'backslash', '"': 'apostrophe'}.get(char)
                if name is None:
                    raise ValueError(f'Unsupported text character {char!r}')
                keys = ([{'type': 'qcode', 'data': 'shift'}] if char in ':_>|"' else []) + [{'type': 'qcode', 'data': name}]
            qmp(a.port, 'send-key', {'keys': keys, 'hold-time': 30})
            time.sleep(0.06)
    else:
        command = {'status': 'query-status', 'powerdown': 'system_powerdown', 'quit': 'quit'}[a.action]
        print(qmp(a.port, command))


if __name__ == '__main__':
    main()
