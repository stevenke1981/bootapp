# BootApp Legacy BIOS backend v0.3

This is a standalone BIOS bootstrap and Rust menu. It does not invoke GRUB or
UEFI to start the manager. Existing OS loaders such as GRUB/Windows MBR or VBR
code are chainloaded after selection; BootApp itself does not load a Linux kernel.

## CPU requirement

**SSE2 is required** (Pentium 4 / Pentium M / Athlon 64 class or newer). The
Rust code links the prebuilt `i686-unknown-linux-gnu` core library, which is
compiled for a Pentium 4 baseline and contains SSE2 instructions (`movsd`,
`movdqa`, `paddd` on xmm registers); avoiding them would need nightly
`build-std`, which this project does not use. `build-bios.py` reports the
count of SSE/SSE2 instructions in `build.json` (`cpu_features`), and stage2
checks CPUID (FXSR, SSE, SSE2) in real mode before enabling SSE: on an older
CPU it prints `BootApp 0.3: CPU lacks FXSR/SSE/SSE2
(BOOTAPP_BIOS_CPU_UNSUPPORTED)` on screen and COM1 and halts instead of
faulting. v0.2 documentation claimed Pentium III; that evidence was invalid
because QEMU TCG executes SSE2 even when the emulated CPUID lacks it.

## Build

Requires Rust stable, `i686-unknown-linux-gnu`, `llvm-tools-preview`, Python and NASM.
No administrator rights or disk-writing utility is required.

```powershell
rustup target add i686-unknown-linux-gnu
rustup component add llvm-tools-preview
python scripts/build-bios.py --nasm <nasm.exe> --output out/bios-v03
```

`--nasm` defaults to `nasm` on `PATH`. `rust-lld` and `llvm-objcopy` are taken
from `<sysroot>/lib/rustlib/<host triple from rustc -vV>/bin` (`.exe` only on
Windows), so the script works on Windows, Linux and macOS hosts. The output
image is new and refuses overwrite; stage2 larger than 120 sectors is rejected
both by the script and by a `link.ld` `ASSERT`. Only NASM's real-mode bootstrap /
firmware thunk is assembly. Menu, scanning, classification, selection and
timeout are Rust. Runtime uses BIOS EDD read services and text mode.

## Menu

Order of entries (the first one is the automatic default):

1. Own boot disk (the BIOS `DL` BootApp started from), see below:
   `Partition N: <kind>` for each primary partition 1-4, then
   `Saved original MBR: <kind>`.
2. Every other BIOS hard disk (`0x80 + n`, at most eight from the BIOS Data
   Area count) with a 0x55aa-signed MBR, in BIOS order: `<kind>` of the disk.

At most 16 entries are kept (rows 5-20), so the screen never exceeds 25 rows.
`<kind>` is `Windows loader` (NTFS VBR with BOOTMGR/NTLDR), `Linux GRUB loader`
(GRUB banner in the boot code) or `Unknown`/`Unknown bootable disk`; unknown
sectors are labelled unknown rather than claimed to contain an OS.

The countdown line shows the live remaining time (`Automatic boot in 5
seconds`, then 4, 3, 2, `1 second`); 91 BIOS ticks at 18.2 Hz are five seconds,
and the tick difference handles the BIOS midnight wrap (`src/timer.rs`). Any key
shows `Automatic boot paused.`; Up/Down select, Enter boots. Each displayed
value is also traced to COM1 as `BOOTAPP_BIOS_COUNTDOWN n`.

### Own-disk entries (install layout)

The installer (CLI, see the top-level docs) writes BootApp into an MBR disk:
LBA0[0..440] = stage1 code, LBA0[440..510] = the disk's own signature and
partition table, LBA1..=120 = stage2, LBA121 = full copy of the original LBA0.
For its own boot drive the manager lists:

- each primary partition 1-4 whose type is not empty/extended (0x05, 0x0F,
  0x85) or GPT protective (0xEE), whose start LBA is ≥ 128 (installer contract;
  this also excludes LBA0 and the stage2 area), and whose start sector is
  0x55aa-signed. Booting it reads that VBR to 0000:7C00, copies the 16-byte
  partition entry exactly as on disk (active flag included) to 0000:0620 and
  enters with DS:SI = 0000:0620 and DL = own drive;
- `Saved original MBR` when LBA121 is 0x55aa-signed, its boot code 0..440 is
  not blank (all 0x00 or all 0xFF, e.g. a disk that never had MBR code) and it
  differs from the current LBA0[0..440]. Booting it reads LBA121 into the
  0x8000 buffer, overwrites bytes 440..510 with the *current* LBA0 disk
  signature and partition table (so partitions changed after installation are
  not booted from the stale install-time copy), and jumps to it through
  `bios_chainload_buffer` without re-reading. DS:SI points at the active entry
  of the current table (zeros if none).

Own LBA0 is never offered (it is BootApp and would recurse). Other drives are
unchanged: their MBR (LBA0) is chainloaded with their own DL and DS:SI pointing
at their active entry. Extended partitions, GPT disks and EFI-only loaders are
not booted through this backend.

## ABI and safety boundary

| Memory | Role |
|---|---|
| 0x0600 | Initial BIOS boot drive byte (stored by stage1) |
| 0x0620..0x062f | Partition entry for DS:SI at chainload |
| 0x7000 | Real-mode BIOS thunk stack (grows down) |
| 0x7c00..0x7dff | Stage1, later the chainloaded MBR/VBR |
| 0x8000..0x81ff | EDD single-sector scratch (`bios_read`/`bios_chainload`) |
| 0x10000..0x1efff | Stage2: bootstrap, thunk data and Rust image (120 sectors); BSS ends ≤ 0x20000 |
| ≤ 0x90000 | Protected-mode Rust stack top (see below) |

Rust ABI (cdecl, 32-bit flat): `bios_read(drive, lba) -> status` (sector to
0x8000), `bios_ticks() -> u32` (INT 1Ah), `bios_key() -> u32` (INT 16h,
0 = none), `bios_chainload(drive, lba) -> status` (returns only on read error),
`bios_boot_drive() -> u32`, `bios_chainload_buffer(drive) -> status` (jumps to
the sector Rust already placed/patched at 0x8000, no disk read; never returns),
and the data word `bios_stack_top`.

The protected-mode stack top is computed in 32-bit code before BSS zeroing:
`min(0x90000, L)` aligned down to 16, where L is the EBDA base (BDA word 0x40E
<< 4) when non-zero and ≥ 0x20000, otherwise the INT 12h conventional-memory
size (queried in real mode) when ≥ 0x20000; with neither, 0x90000 is kept. It
is traced as `BOOTAPP_BIOS_STACK top=…` (0x00090000 under SeaBIOS, whose EBDA
is at 0x9FC00). The linker asserts BSS ends ≤ 0x20000, below any accepted top.

Stage1 uses EDD AH=42 to load 120 sectors at LBA1 into 1000:0000. A20 is
enabled, flat protected-mode segments and SSE state are initialized, and BSS is
zeroed before calling Rust. BIOS calls go 32-bit → 16-bit protected (selector
0x18 code base 0x10000, 0x20 data base 0, both limit 0xFFFF) → real mode
(CS=DS=0x1000) on a separate stack and restore the protected-mode stack and
callee-saved registers. No paging, hardware interrupt handlers or OS services
are installed. At chainload, a freshly read sector is copied to 0000:7c00, PE is
clear, DS=ES=SS=FS=GS=0, SP=7c00, DL is the chosen drive, DS:SI=0000:0620, DF=0
and IF=1.

**Real-mode addressing rule.** Real-mode and 16-bit code never uses an
effective offset above 0xFFFF: its data is addressed as
`[label - image_start]` with DS (or CS) = 0x1000, never as `[dword label]`
(linear 0x10xxx, #GP on real CPUs/KVM/WHPX; QEMU TCG does not enforce
real-mode limits and hid the v0.2 bug). `link.ld` asserts the `.start` section
ends below 0x20000. `scripts/check-bios-realmode.py` is the static regression
lint (method and limits in its docstring):

```powershell
python scripts/check-bios-realmode.py --ndisasm <ndisasm.exe> [--elf out/bios/stage2.elf]
```

Scanning is bounded to the BIOS Data Area disk count, maximum eight drives,
plus four partitions and one saved MBR on the own disk. A missing UART cannot
hang output. The manager has no write-sector service and performs no host
boot-setting mutation. Build writes only image files.

## Validation

`scripts/test-bios.py --case X` (TCG by default, `--accel whpx|kvm` where
available, `--out DIR`):

| Case | Proves |
|---|---|
| `auto` | other-disk MBR chainload with DL=81 after live countdown 5..1 |
| `keyboard` | Esc cancels the countdown, Enter boots |
| `empty` | no entry and no chainload without a candidate |
| `own-partition` | installer-layout disk: partition VBR at LBA 2048 auto-boots with DL=80, SI=0620 and the exact on-disk partition entry |
| `saved-mbr` | same disk but LBA121 holds a stale table: Down+Enter boots the saved MBR, which sees the current partition entry |
| `saved-mbr-blank` | LBA121 code area all zero: saved MBR not listed, partition auto-boots |
| `no-sse2` | `-cpu pentium3` (no SSE2): stage2 refuses with `BOOTAPP_BIOS_CPU_UNSUPPORTED` |

TCG cases run with `-cpu pentium3,+sse2` (QEMU 11 has no `pentium4` model; this
is the minimal FXSR+SSE+SSE2 CPU); `--accel whpx|kvm` uses `-cpu host`.

Fixtures (`asm/fixture.asm`, `asm/vbr-fixture.asm`) are not operating systems.
Only QEMU TCG was run for v0.3; WHPX/KVM and physical hardware are NOT_RUN, so
the real-mode limit fix is verified statically (lint) and functionally under
TCG only. The EBDA/INT 12h stack-limit branches below 0x90000 are reviewed but
not exercised (SeaBIOS always yields the 0x90000 ceiling). Baseline Rust target
is i686 Linux ABI with freestanding linking; no Linux calls are made. Firmware
EDD, text mode and keyboard services and an SSE2 CPU are required.
