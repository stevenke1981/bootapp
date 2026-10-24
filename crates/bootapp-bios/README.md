# BootApp Legacy BIOS backend v0.2

This is a standalone BIOS bootstrap and Rust menu. It does not invoke GRUB or
UEFI to start the manager. Existing OS loaders such as GRUB/Windows MBR code are
chainloaded after selection; BootApp itself does not load a Linux kernel.

## Build

Requires Rust stable, `i686-unknown-linux-gnu`, `llvm-tools-preview`, Python and NASM.
No administrator rights or disk-writing utility is required.

```powershell
rustup target add i686-unknown-linux-gnu
rustup component add llvm-tools-preview
python scripts/build-bios.py --nasm <nasm.exe> --output out/bios-v02
```

The output image is new and refuses overwrite. The build uses the existing Rust
core archive, freestanding panic=abort Rust, rust-lld and llvm-objcopy. Only NASM's
real-mode bootstrap / firmware thunk is assembly. Menu, scanning, classification,
selection and timeout are Rust. Runtime uses BIOS EDD read services and text mode.

## Use in QEMU

Attach `bootapp-bios.img` as IDE index0, an OS disk copy/overlay as IDE index1.
The manager skips its own BIOS drive and offers other disks with signed sectors.
Default selection automatically boots after five seconds. Up/Down selects, Enter
boots, any key cancels the countdown. The UI shows disk index and loader evidence.
Unknown bootable disks are labelled unknown, rather than claimed to contain an OS.

Every attached candidate requires a 0x55aa signed MBR. GRUB's code banner indicates
a Linux GRUB loader; an NTFS VBR and BOOTMGR/NTLDR marker indicate a Windows loader.
Primary partition VBRs are inspected for classification; BIOS chainloading starts
the selected disk MBR with its actual BIOS DL. MBR code is responsible for active
partition selection, extended partitions and its subsequent loader stages.
Firmware-only EFI/macOS loaders cannot run through this BIOS backend.

## ABI and safety boundary

| Memory | Role |
|---|---|
| 0x0600 | Initial BIOS boot drive byte |
| 0x0620..0x062f | Copied active primary partition entry for DS:SI |
| 0x7000 | Real-mode BIOS thunk stack |
| 0x7c00..0x7dff | Chainloaded MBR |
| 0x8000..0x81ff | EDD single-sector scratch |
| 0x10000..0x1efff | Maximum bootstrap + Rust image |
| 0x90000 | Protected-mode Rust stack top |

Stage1 uses EDD AH=42 to load 120 sectors at LBA1 into 0x10000. The linker bounds
the image to that reserved range. A20 is enabled, flat protected-mode segments
and SSE state are initialized, and BSS is zeroed before calling Rust. BIOS calls
switch to real mode on a separate stack and restore the protected-mode stack and
callee-saved registers. No paging, hardware interrupt handlers or OS services are
installed. At chainload, a freshly read sector is copied to 0000:7c00, PE is clear,
DS=ES=SS=FS=GS=0, SP=7c00, DL is the chosen drive, DS:SI=0000:0620, DF=0 and IF=1.
The fixture verifies real mode, DL and DS/ES/SS before printing its success marker.

Scanning is bounded to the BIOS Data Area disk count, maximum eight drives. A
missing UART cannot hang output. Manager has no write-sector service, installer,
physical-device command or host boot-setting mutation. Build writes only image files.

## Validation

`scripts/test-bios.py` supports `auto`, `keyboard`, and `empty` fixture cases.
These tests prove Rust menu boot, ABI-correct disk0x81 chainload and timeout
cancellation; the fixture is not a real OS. Actual Linux/Windows acceptance is
the integration owner's separate responsibility. Physical hardware NOT_RUN.

Baseline Rust target is i686 Linux ABI with freestanding linking; no Linux calls
are made. QEMU Pentium III fixture boot passed, but firmware EDD, text mode,
keyboard services and the prebuilt Rust core's CPU requirements still apply.
