#!/usr/bin/env python3
"""Secure Boot signing helper for BootApp EFI binaries (stdlib only).

Subcommands:
  keys    --out DIR [--cn NAME]                  create a NEW directory with throwaway
                                                 PK/KEK/db keys, certs, .esl and .auth files
  sign    --key K --cert C --in EFI --out NEW    sign one EFI binary with sbsign
  verify  --cert C --in EFI                      check a signature with sbverify

Safety rules:
  * `keys` refuses to write into a directory that already exists.
  * `sign` refuses to overwrite its output and refuses to sign in place.
  * Nothing here touches firmware, NVRAM, the EFI System Partition or any disk.
    Enrolling PK/KEK/db keys is a separate, manual firmware step; see
    docs/SECURE-BOOT.md.
  * Keys made by `keys` are for testing only. Private keys (*.key) are written
    WITHOUT a passphrase. Never commit them and never reuse them on real hardware
    without a deliberate decision.

External tools (not bundled, not installed by this script):
  openssl, sbsign, sbverify, cert-to-efi-sig-list, sign-efi-sig-list
  Debian/Ubuntu: apt install openssl sbsigntool efitools
  Windows: run the script inside WSL (Ubuntu) or MSYS2 where those packages exist.
  If a tool is missing the script prints what to install and exits with status 3.
"""

import argparse
import os
import pathlib
import re
import shutil
import subprocess
import sys
import time
import uuid

EXIT_MISSING_TOOL = 3
PACKAGES = 'openssl sbsigntool efitools'
CN_RE = re.compile(r'^[A-Za-z0-9 ._-]{1,64}$')
KEY_NAMES = ('PK', 'KEK', 'db')
CERT_DAYS = '3650'


def require_tools(names):
    """Exit with status 3 and an install hint if any required tool is absent."""
    missing = [name for name in names if shutil.which(name) is None]
    if not missing:
        return
    print('error: required tool(s) not found: ' + ', '.join(missing), file=sys.stderr)
    if os.name == 'nt':
        print('This script runs on Linux tooling. On Windows install the tools inside WSL '
              '(apt install ' + PACKAGES + ') or MSYS2, then run this script from that environment.',
              file=sys.stderr)
    else:
        print('Install them, for example: apt install ' + PACKAGES, file=sys.stderr)
    sys.exit(EXIT_MISSING_TOOL)


def run(cmd):
    """Run a tool with an argument list (never a shell). Raise SystemExit on failure."""
    argv = [str(part) for part in cmd]
    print('+ ' + ' '.join(argv), flush=True)
    result = subprocess.run(argv, check=False)
    if result.returncode != 0:
        raise SystemExit('error: ' + argv[0] + ' failed with exit code ' + str(result.returncode))


def require_file(path, label):
    if not path.is_file():
        raise SystemExit('error: ' + label + ' not found: ' + str(path))


def cmd_keys(args):
    require_tools(['openssl', 'cert-to-efi-sig-list', 'sign-efi-sig-list'])
    if not CN_RE.match(args.cn):
        raise SystemExit('error: --cn must be 1-64 characters from letters, digits, space, dot, underscore, hyphen')
    out = args.out
    if out.exists():
        raise SystemExit('error: ' + str(out) + ' already exists; choose a new directory (refusing to overwrite)')
    if not out.parent.is_dir():
        raise SystemExit('error: parent directory does not exist: ' + str(out.parent))

    out.mkdir()
    try:
        guid = str(uuid.uuid4())
        stamp = time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime())
        for name in KEY_NAMES:
            key = out / (name + '.key')
            crt = out / (name + '.crt')
            run(['openssl', 'req', '-new', '-x509', '-newkey', 'rsa:2048', '-sha256',
                 '-days', CERT_DAYS, '-nodes',
                 '-subj', '/CN=' + args.cn + ' ' + name,
                 '-keyout', key, '-out', crt])
            try:
                os.chmod(key, 0o600)
            except OSError:
                pass  # Windows ignores POSIX modes; the directory is still the user's own.
            run(['cert-to-efi-sig-list', '-g', guid, crt, out / (name + '.esl')])

        # PK signs itself; KEK is signed by PK; db is signed by KEK.
        signers = {'PK': 'PK', 'KEK': 'PK', 'db': 'KEK'}
        for name in KEY_NAMES:
            signer = signers[name]
            run(['sign-efi-sig-list', '-k', out / (signer + '.key'), '-c', out / (signer + '.crt'),
                 '-t', stamp, name, out / (name + '.esl'), out / (name + '.auth')])
    except BaseException:
        shutil.rmtree(out, ignore_errors=True)  # remove the partial directory we created
        raise

    print('keys written to ' + str(out))
    print('Test keys only. Enrollment is manual; see docs/SECURE-BOOT.md.')


def cmd_sign(args):
    require_tools(['sbsign'])
    require_file(args.key, 'signing key')
    require_file(args.cert, 'certificate')
    require_file(args.input, 'input EFI binary')
    if args.output.exists():
        raise SystemExit('error: output already exists (refusing to overwrite): ' + str(args.output))
    if args.output.resolve() == args.input.resolve():
        raise SystemExit('error: --out must differ from --in (no signing in place)')
    if not args.output.parent.is_dir():
        raise SystemExit('error: output directory does not exist: ' + str(args.output.parent))
    run(['sbsign', '--key', args.key, '--cert', args.cert, '--output', args.output, args.input])
    print('signed: ' + str(args.output))


def cmd_verify(args):
    require_tools(['sbverify'])
    require_file(args.cert, 'certificate')
    require_file(args.input, 'EFI binary')
    run(['sbverify', '--cert', args.cert, args.input])
    print('verify: OK (signature chains to ' + str(args.cert) + ')')


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)

    p_keys = sub.add_parser('keys', help='create a new directory with throwaway PK/KEK/db keys')
    p_keys.add_argument('--out', required=True, type=pathlib.Path, help='new directory to create')
    p_keys.add_argument('--cn', default='BootApp Test', help='certificate common-name prefix')
    p_keys.set_defaults(func=cmd_keys)

    p_sign = sub.add_parser('sign', help='sign an EFI binary with sbsign')
    p_sign.add_argument('--key', required=True, type=pathlib.Path, help='private key (PEM)')
    p_sign.add_argument('--cert', required=True, type=pathlib.Path, help='certificate (PEM)')
    p_sign.add_argument('--in', dest='input', required=True, type=pathlib.Path, help='unsigned EFI binary')
    p_sign.add_argument('--out', dest='output', required=True, type=pathlib.Path, help='new signed output file')
    p_sign.set_defaults(func=cmd_sign)

    p_verify = sub.add_parser('verify', help='check a signature with sbverify')
    p_verify.add_argument('--cert', required=True, type=pathlib.Path, help='expected signing certificate (PEM)')
    p_verify.add_argument('--in', dest='input', required=True, type=pathlib.Path, help='EFI binary to check')
    p_verify.set_defaults(func=cmd_verify)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    args.func(args)
    return 0


if __name__ == '__main__':
    sys.exit(main())
