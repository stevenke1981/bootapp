"""Build a new BIOS image; writes only the supplied output directory."""
import argparse,pathlib,subprocess,shutil,json,hashlib

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--nasm',required=True,type=pathlib.Path)
    p.add_argument('--output',type=pathlib.Path,default=pathlib.Path('out/bios'))
    a=p.parse_args(); root=pathlib.Path(__file__).resolve().parents[1]
    crate=root/'crates/bootapp-bios'; out=a.output.resolve();out.mkdir(parents=True,exist_ok=True)
    if (out/'bootapp-bios.img').exists():raise SystemExit('refusing to overwrite existing BIOS image')
    sysroot=pathlib.Path(subprocess.check_output(['rustc','--print','sysroot'],text=True).strip())
    tools=sysroot/'lib/rustlib/x86_64-pc-windows-msvc/bin'
    def run(args):subprocess.run([str(x) for x in args],check=True,cwd=crate)
    run([a.nasm,'-f','bin',crate/'asm/stage1.asm','-o',out/'stage1.bin'])
    run([a.nasm,'-f','elf32',crate/'asm/stage2.asm','-o',out/'stage2.o'])
    run(['rustc','--edition=2024','--target=i686-unknown-linux-gnu','--crate-type=staticlib',
         '-C','opt-level=s','-C','panic=abort','-C','relocation-model=static',
         crate/'src/main.rs','-o',out/'rust.a'])
    run([tools/'rust-lld.exe','-flavor','gnu','-m','elf_i386','--gc-sections','-T',crate/'link.ld',out/'stage2.o',out/'rust.a','-o',out/'stage2.elf'])
    run([tools/'llvm-objcopy.exe','-O','binary',out/'stage2.elf',out/'stage2.bin'])
    stage1=(out/'stage1.bin').read_bytes();stage2=(out/'stage2.bin').read_bytes()
    assert len(stage1)==512 and stage1[-2:]==b'\x55\xaa'
    if len(stage2)>120*512:raise SystemExit('stage2 exceeds reserved BIOS stage1 load window')
    image=stage1+stage2+bytes(120*512-len(stage2))
    image+=bytes(1024*1024-len(image))
    (out/'bootapp-bios.img').write_bytes(image)
    (out/'build.json').write_text(json.dumps({'image_sha256':hashlib.sha256(image).hexdigest(),'stage2_bytes':len(stage2),'image_bytes':len(image)},indent=2))
    print((out/'build.json').read_text())

if __name__=='__main__':main()
