"""Build a new BIOS image; writes only the supplied output directory."""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess

EXE = '.exe' if os.name == 'nt' else ''
STAGE2_SECTORS = 120  # stage1 loads LBA 1..=120 to 0x10000 (see stage1.asm)
# SSE (Pentium III) mnemonics operating on xmm registers; any other xmm
# instruction is counted as SSE2 or later.
SSE1 = {'movaps', 'movups', 'movss', 'movlps', 'movhps', 'movhlps', 'movlhps', 'movntps', 'xorps',
        'andps', 'andnps', 'orps', 'addps', 'addss', 'subps', 'subss', 'mulps', 'mulss', 'divps',
        'divss', 'sqrtps', 'sqrtss', 'maxps', 'maxss', 'minps', 'minss', 'cmpps', 'cmpss', 'comiss',
        'ucomiss', 'shufps', 'unpcklps', 'unpckhps', 'cvtsi2ss', 'cvtss2si', 'cvttss2si', 'cvtpi2ps',
        'cvtps2pi', 'cvttps2pi', 'rcpps', 'rcpss', 'rsqrtps', 'rsqrtss', 'movmskps'}


def host_tools():
    """rust-lld / llvm-objcopy from <sysroot>/lib/rustlib/<host>/bin (llvm-tools)."""
    sysroot = pathlib.Path(subprocess.check_output(['rustc', '--print', 'sysroot'], text=True).strip())
    version = subprocess.check_output(['rustc', '-vV'], text=True)
    host = next((line.split(':', 1)[1].strip() for line in version.splitlines()
                 if line.startswith('host:')), None)
    if not host:
        raise SystemExit('cannot determine host triple from `rustc -vV`')
    bin_dir = sysroot / 'lib' / 'rustlib' / host / 'bin'
    tools = {name: bin_dir / (name + EXE) for name in ('rust-lld', 'llvm-objcopy', 'llvm-objdump')}
    for name, path in tools.items():
        if not path.is_file():
            raise SystemExit(f'{name} not found at {path}; run `rustup component add llvm-tools-preview`')
    return tools


def cpu_features(objdump, elf):
    """Informational: count SSE / SSE2+ xmm instructions in the Rust .text."""
    listing = subprocess.run([str(objdump), '-d', '--no-show-raw-insn', '--x86-asm-syntax=intel',
                              '-j', '.text', str(elf)], check=True, capture_output=True, text=True).stdout
    sse = sse2 = 0
    for line in listing.splitlines():
        head, sep, insn = line.partition(':')
        if not sep or not head.strip() or 'xmm' not in insn:
            continue
        mnemonic = insn.split()[0]
        if mnemonic in SSE1:
            sse += 1
        else:
            sse2 += 1
    required = 'SSE2' if sse2 else ('SSE' if sse else 'none beyond i686')
    return {'sse_instructions': sse, 'sse2_or_later_instructions': sse2, 'required_extension': required}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--nasm', type=pathlib.Path, help='nasm executable (default: nasm on PATH)')
    p.add_argument('--output', type=pathlib.Path, default=pathlib.Path('out/bios'))
    a = p.parse_args()
    nasm = a.nasm or (pathlib.Path(found) if (found := shutil.which('nasm')) else None)
    if nasm is None or not nasm.is_file():
        raise SystemExit('nasm not found; pass --nasm PATH')
    root = pathlib.Path(__file__).resolve().parents[1]
    crate = root / 'crates' / 'bootapp-bios'
    out = a.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / 'bootapp-bios.img').exists():
        raise SystemExit('refusing to overwrite existing BIOS image')
    tools = host_tools()

    def run(args):
        subprocess.run([str(x) for x in args], check=True, cwd=crate)

    run([nasm, '-f', 'bin', crate / 'asm/stage1.asm', '-o', out / 'stage1.bin'])
    run([nasm, '-f', 'elf32', crate / 'asm/stage2.asm', '-o', out / 'stage2.o'])
    run(['rustc', '--edition=2024', '--target=i686-unknown-linux-gnu', '--crate-type=staticlib',
         '-C', 'opt-level=s', '-C', 'panic=abort', '-C', 'relocation-model=static',
         crate / 'src/main.rs', '-o', out / 'rust.a'])
    run([tools['rust-lld'], '-flavor', 'gnu', '-m', 'elf_i386', '--gc-sections', '-T', crate / 'link.ld',
         out / 'stage2.o', out / 'rust.a', '-o', out / 'stage2.elf'])
    run([tools['llvm-objcopy'], '-O', 'binary', out / 'stage2.elf', out / 'stage2.bin'])
    stage1 = (out / 'stage1.bin').read_bytes()
    stage2 = (out / 'stage2.bin').read_bytes()
    if len(stage1) != 512 or stage1[-2:] != b'\x55\xaa':
        raise SystemExit('stage1 must be one 0x55aa-signed sector')
    if len(stage2) > STAGE2_SECTORS * 512:
        raise SystemExit('stage2 exceeds reserved BIOS stage1 load window')
    image = stage1 + stage2 + bytes(STAGE2_SECTORS * 512 - len(stage2))
    image += bytes(1024 * 1024 - len(image))
    features = cpu_features(tools['llvm-objdump'], out / 'stage2.elf')
    (out / 'bootapp-bios.img').write_bytes(image)
    (out / 'build.json').write_text(json.dumps({'image_sha256': hashlib.sha256(image).hexdigest(),
                                                'stage2_bytes': len(stage2), 'image_bytes': len(image),
                                                'cpu_features': features}, indent=2))
    print((out / 'build.json').read_text())


if __name__ == '__main__':
    main()
