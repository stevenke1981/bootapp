"""Static lint: BIOS real-mode / 16-bit code must not form offsets above 0xFFFF.

Why: in real mode (and in 16-bit protected mode with the 0xFFFF-limit data
descriptor BootApp uses) every data access is checked against the segment limit
0xFFFF. An operand such as ``[dword label]`` whose 32-bit displacement is a
linear address >= 0x10000 raises #GP on real CPUs and under KVM/WHPX, while
QEMU TCG does not enforce real-mode limits and silently works. This lint is the
regression guard for that class of bug.

Method
  1. Assemble the source with NASM (``-l`` listing). Stage2 is assembled as
     ELF32; its ``.start`` section is relocated in Python at the link base taken
     from ``link.ld`` (``. = 0x10000;``) by applying R_386_32/PC32/16/PC16
     relocations; undefined externs get the placeholder 0x40000 (so a 16-bit
     use of an extern is reported, conservatively). With ``--elf`` the bytes of
     the real linked ``stage2.elf`` are used instead and must match the
     assembled object outside extern relocation fields. Stage1 is assembled as
     a flat binary at its ``org``.
  2. The source is scanned line by line for ``bits 16|32|64``/``use16|32`` and
     ``section`` directives; each listing line (line number -> offset) is
     attributed to its mode. Data directives (db/dw/dd/dq/times/align/res*) are
     not code. Contiguous 16-bit code lines form the checked regions.
  3. Each region is disassembled with ``ndisasm -b 16 -o <linear>``. Every
     listing offset inside a region must be an ndisasm instruction start
     (otherwise the disassembly desynchronised and the lint fails).
  4. A memory operand fails when it uses 32-bit address size (0x67 prefix):
     a pure displacement fails if disp + access_size - 1 > 0xFFFF; any 32-bit
     base/index register form fails because its range cannot be proven.
     16-bit address-size operands wrap modulo 64 KiB and always pass.
     Each 16-bit code byte must also lie within 0xFFFF of the image base (CS
     limit of both the real-mode CS=0x1000 and the 0x18 code descriptor).

Limits (honest): segment register *values* are not tracked, so a 16-bit offset
used with the wrong DS (e.g. ``[label - image_start]`` while DS=0) reads the
wrong memory without being reported - the QEMU fixture tests cover those paths
functionally. Mode tracking is lexical: directives hidden in macros, %include
files, or 16-bit code outside stage2's ``.start`` section are not supported
(the script errors on 16-bit code in other sections). Implicit stack/string
accesses (push, rep movs) use 16-bit SP/SI/DI and are not checked. The source
must be assembled to the bytes that run: pass the same file that was built.

Usage
  python scripts/check-bios-realmode.py --ndisasm PATH [--nasm PATH]
         [--stage2 asm] [--elf stage2.elf] [--stage1 asm] [--link link.ld]
Exit 0 = PASS, 1 = violations found, 2 = tool/input error. Stdlib only.
"""
import argparse
import os
import pathlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile

EXE = '.exe' if os.name == 'nt' else ''
ROOT = pathlib.Path(__file__).resolve().parents[1]
CRATE = ROOT / 'crates' / 'bootapp-bios'
EXTERN_PLACEHOLDER = 0x40000
LIMIT = 0xFFFF
DATA_DIRECTIVES = {'db', 'dw', 'dd', 'dq', 'dt', 'do', 'dy', 'dz', 'times', 'align', 'alignb',
                   'resb', 'resw', 'resd', 'resq', 'rest', 'reso', 'incbin', 'equ'}
REG32 = re.compile(r'\b(e[abcd]x|e[sd]i|e[sb]p)\b')
PREFIXES = {0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65, 0x66, 0x67, 0xF0, 0xF2, 0xF3}


class ToolError(Exception):
    pass


def tool(explicit, name):
    if explicit:
        path = pathlib.Path(explicit)
        if not path.is_file():
            raise ToolError(f'{name} not found: {path}')
        return path
    found = shutil.which(name + EXE) or shutil.which(name)
    if not found:
        raise ToolError(f'{name} not on PATH; pass --{name}')
    return pathlib.Path(found)


def strip_comment(line):
    out, quote = [], None
    for ch in line:
        if quote:
            if ch == quote:
                quote = None
        elif ch in '\'"`':
            quote = ch
        elif ch == ';':
            break
        out.append(ch)
    return ''.join(out).strip()


def source_states(lines):
    """Per source line (1-based): (bits, section, is_code)."""
    bits, section, states = 16, None, {}
    for number, raw in enumerate(lines, 1):
        text = strip_comment(raw)
        text = re.sub(r'^[A-Za-z_.?$][\w.?$@#~]*:\s*', '', text)  # label
        directive = text.strip('[] ').lower()
        words = directive.split()
        code = bool(words)
        if words:
            if words[0] == 'bits' and len(words) > 1:
                bits = int(words[1], 0)
                code = False
            elif words[0] in ('use16', 'use32', 'use64'):
                bits = int(words[0][3:])
                code = False
            elif words[0] in ('section', 'segment') and len(words) > 1:
                section = words[1]
                code = False
            elif words[0] in ('org', 'global', 'extern', 'default', 'cpu', 'absolute',
                              'common', 'static') or words[0].startswith('%'):
                code = False
            elif words[0] in DATA_DIRECTIVES or (len(words) > 1 and words[1] in DATA_DIRECTIVES):
                code = False
        states[number] = (bits, section, code)
    return states


def listing_offsets(path):
    """[(source_line, offset)] for every listing line that emitted bytes."""
    rows = []
    for line in path.read_text(encoding='utf-8', errors='replace').splitlines():
        m = re.match(r'^\s*(\d+)\s+([0-9A-F]{8})\s+\S', line)
        if m and '<' not in line[:8]:
            rows.append((int(m.group(1)), int(m.group(2), 16)))
    return rows


# ---------------------------------------------------------------- ELF32 helpers
def elf_sections(data):
    if data[:4] != b'\x7fELF' or data[4] != 1 or data[5] != 1:
        raise ToolError('not a little-endian ELF32 file')
    shoff, = struct.unpack_from('<I', data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from('<HHH', data, 0x2E)
    raw = [struct.unpack_from('<10I', data, shoff + i * shentsize) for i in range(shnum)]
    strtab = raw[shstrndx]
    names = []
    for header in raw:
        start = strtab[4] + header[0]
        names.append(data[start:data.index(b'\0', start)].decode())
    keys = ('name', 'type', 'flags', 'addr', 'offset', 'size', 'link', 'info', 'align', 'entsize')
    return [dict(zip(keys, header), label=name, index=i) for i, (header, name) in enumerate(zip(raw, names))]


def elf_section(sections, label):
    for section in sections:
        if section['label'] == label:
            return section
    raise ToolError(f'ELF section {label} missing')


def relocate_object(obj, base):
    """Return (.start bytes at base, set of byte offsets covered by extern relocations)."""
    sections = elf_sections(obj)
    start = elf_section(sections, '.start')
    code = bytearray(obj[start['offset']:start['offset'] + start['size']])
    extern_bytes = set()
    for rel in sections:
        if rel['type'] != 9 or rel['info'] != start['index']:  # SHT_REL for .start
            continue
        symtab = sections[rel['link']]
        for i in range(rel['size'] // 8):
            r_offset, r_info = struct.unpack_from('<II', obj, rel['offset'] + i * 8)
            sym, kind = r_info >> 8, r_info & 0xFF
            _, value, _, _, _, shndx = struct.unpack_from('<IIIBBH', obj, symtab['offset'] + sym * 16)
            if shndx == start['index']:
                target = base + value
            elif shndx == 0:
                target = EXTERN_PLACEHOLDER
                extern_bytes.update(range(r_offset, r_offset + 4))
            elif shndx == 0xFFF1:
                target = value
            else:
                raise ToolError(f'relocation against unsupported section index {shndx}')
            place = base + r_offset
            if kind in (1, 2):
                addend, = struct.unpack_from('<i', code, r_offset)
                result = target + addend - (place if kind == 2 else 0)
                struct.pack_into('<I', code, r_offset, result & 0xFFFFFFFF)
            elif kind in (20, 21):
                addend, = struct.unpack_from('<h', code, r_offset)
                result = target + addend - (place if kind == 21 else 0)
                struct.pack_into('<H', code, r_offset, result & 0xFFFF)
            else:
                raise ToolError(f'unsupported relocation type {kind}')
    return bytes(code), extern_bytes


# ------------------------------------------------------------------ analysis
def operand_size(asm, mnemonic):
    if mnemonic in ('lgdt', 'lidt', 'sgdt', 'sidt'):
        return 6
    sizes = {'byte': 1, 'word': 2, 'dword': 4, 'qword': 8, 'tword': 10}
    before = asm.split('[', 1)[0].split()
    for word in reversed(before):
        if word in sizes:
            return sizes[word]
    if re.search(r'\be[a-d]x\b|\be[sd]i\b', asm):
        return 4
    if re.search(r'\b[a-d]x\b|\b[sd]i\b', asm):
        return 2
    return 1


def check_region(ndisasm, code, base, begin, end, listing_points, where, problems, tmp):
    if end - base > LIMIT + 1:
        problems.append(f'{begin:#07x}..{end:#07x}: 16-bit code beyond CS limit 0xFFFF of base {base:#x}')
    blob = tmp / f'region-{begin:x}.bin'
    blob.write_bytes(code[begin - base:end - base])
    out = subprocess.run([str(ndisasm), '-b', '16', '-o', hex(begin), str(blob)],
                         check=True, capture_output=True, text=True).stdout
    starts = set()
    for line in out.splitlines():
        m = re.match(r'^([0-9A-F]{8})\s+([0-9A-F]+)\s+(.*)$', line)
        if not m:
            continue
        address, raw, asm = int(m.group(1), 16), bytes.fromhex(m.group(2)), m.group(3).strip()
        starts.add(address)
        if '[' not in asm:
            continue
        prefix = set()
        for byte in raw:
            if byte not in PREFIXES:
                break
            prefix.add(byte)
        a32 = 0x67 in prefix
        mnemonic = asm.split()[0]
        for expression in re.findall(r'\[([^\]]*)\]', asm):
            body = re.sub(r'\b(byte|word|dword|qword|tword|[cdefgs]s:)\s*', '', expression).strip()
            if not a32 and not REG32.search(body):
                continue  # 16-bit effective address wraps within 64 KiB
            source = where(address)
            if REG32.search(body):
                problems.append(f'{address:#010x} {asm} ({source}): 32-bit register address in 16-bit code')
                continue
            try:
                disp = int(body, 0)
            except ValueError:
                problems.append(f'{address:#010x} {asm} ({source}): unparsed 32-bit address')
                continue
            last = disp + operand_size(asm, mnemonic) - 1
            if last > LIMIT:
                problems.append(f'{address:#010x} {asm} ({source}): effective offset {disp:#x}..{last:#x} > 0xFFFF')
    for point in listing_points:
        if begin <= point < end and point not in starts:
            problems.append(f'{point:#010x}: ndisasm desynchronised (data inside 16-bit code?) at {where(point)}')


def regions(states, rows, size, base, want_section):
    """Merge listing rows into 16-bit code [begin, end) ranges (linear)."""
    rows = sorted((offset, line) for line, offset in rows)
    spans = []
    for i, (offset, line) in enumerate(rows):
        bits, section, code = states.get(line, (32, None, False))
        nxt = rows[i + 1][0] if i + 1 < len(rows) else size
        if bits != 16 or not code or nxt <= offset:
            continue
        if want_section is not None and section != want_section:
            raise ToolError(f'16-bit code outside {want_section} at source line {line}: unsupported')
        begin, end = base + offset, base + nxt
        if spans and spans[-1][1] == begin:
            spans[-1][1] = end
        else:
            spans.append([begin, end])
    return spans, [base + offset for offset, _ in rows]


def lint(label, source, code, base, rows, ndisasm, section, problems, tmp):
    lines = source.read_text(encoding='utf-8', errors='replace').splitlines()
    states = source_states(lines)
    by_linear = sorted((base + offset, line) for line, offset in rows)

    def where(address):
        best = None
        for linear, line in by_linear:
            if linear <= address:
                best = line
        return f'{source.name}:{best} {lines[best - 1].strip()}' if best else source.name

    spans, points = regions(states, rows, len(code), base, section)
    if not spans:
        raise ToolError(f'{label}: no 16-bit code found')
    before = len(problems)
    for begin, end in spans:
        check_region(ndisasm, code, base, begin, end, points, where, problems, tmp)
    print(f'{label}: {len(spans)} 16-bit region(s): ' +
          ', '.join(f'{b:#x}..{e:#x}' for b, e in spans) +
          f' -> {len(problems) - before} violation(s)')


def link_base(path):
    m = re.search(r'\.\s*=\s*(0x[0-9a-fA-F]+|\d+)\s*;', path.read_text(encoding='utf-8', errors='replace'))
    if not m:
        raise ToolError(f'no ". = <base>;" in {path}')
    return int(m.group(1), 0)


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    p.add_argument('--ndisasm', help='ndisasm executable (default: PATH)')
    p.add_argument('--nasm', help='nasm executable (default: next to ndisasm, then PATH)')
    p.add_argument('--stage2', type=pathlib.Path, default=CRATE / 'asm' / 'stage2.asm')
    p.add_argument('--elf', type=pathlib.Path, help='linked stage2.elf built from --stage2 (optional)')
    p.add_argument('--stage1', type=pathlib.Path, default=CRATE / 'asm' / 'stage1.asm',
                   help="stage1 source; pass '-' to skip")
    p.add_argument('--link', type=pathlib.Path, default=CRATE / 'link.ld')
    a = p.parse_args()
    try:
        ndisasm = tool(a.ndisasm, 'ndisasm')
        nasm_default = ndisasm.with_name('nasm' + EXE) if not a.nasm else None
        nasm = tool(a.nasm or (nasm_default if nasm_default and nasm_default.is_file() else None), 'nasm')
        base = link_base(a.link)
        problems = []
        with tempfile.TemporaryDirectory(prefix='bios-realmode-') as tmpname:
            tmp = pathlib.Path(tmpname)
            obj, lst = tmp / 'stage2.o', tmp / 'stage2.lst'
            subprocess.run([str(nasm), '-f', 'elf32', str(a.stage2), '-l', str(lst), '-o', str(obj)], check=True)
            code, extern_bytes = relocate_object(obj.read_bytes(), base)
            if a.elf:
                sections = elf_sections(a.elf.read_bytes())
                start = elf_section(sections, '.start')
                linked = a.elf.read_bytes()[start['offset']:start['offset'] + start['size']]
                if start['addr'] != base:
                    raise ToolError(f'.start linked at {start["addr"]:#x}, link.ld says {base:#x}')
                if len(linked) != len(code) or any(x != y for i, (x, y) in enumerate(zip(linked, code))
                                                   if i not in extern_bytes):
                    raise ToolError(f'{a.elf} .start does not match {a.stage2}; rebuild or pass the same source')
                code = linked
            lint(f'stage2 {a.stage2.name}' + (f' ({a.elf.name})' if a.elf else ''), a.stage2, code, base,
                 listing_offsets(lst), ndisasm, '.start', problems, tmp)
            if str(a.stage1) != '-':
                text = a.stage1.read_text(encoding='utf-8', errors='replace')
                m = re.search(r'^\s*\[?org\s+(0x[0-9a-fA-F]+|\d+)', text, re.M | re.I)
                origin = int(m.group(1), 0) if m else 0
                binary, lst1 = tmp / 'stage1.bin', tmp / 'stage1.lst'
                subprocess.run([str(nasm), '-f', 'bin', str(a.stage1), '-l', str(lst1), '-o', str(binary)], check=True)
                lint(f'stage1 {a.stage1.name}', a.stage1, binary.read_bytes(), origin,
                     listing_offsets(lst1), ndisasm, None, problems, tmp)
    except (ToolError, subprocess.CalledProcessError, OSError) as error:
        print(f'ERROR: {error}', file=sys.stderr)
        return 2
    for problem in problems:
        print('FAIL ' + problem)
    print('RESULT: ' + ('FAIL' if problems else 'PASS'))
    return 1 if problems else 0


if __name__ == '__main__':
    raise SystemExit(main())
