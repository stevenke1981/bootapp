# Architecture (v0.3)

BootApp is a Rust boot manager with independent BIOS and UEFI frontends. It
chainloads existing OS loaders. It is not a GRUB script interpreter and not a complete
native Linux/Multiboot kernel loader. v0.3 adds a documented GRUB `grub.cfg` subset,
a host installer, and firmware BootOrder management.

| Component | Responsibility |
|---|---|
| bootapp-core | no_std + alloc: config parser (`lib.rs`), GRUB subset parser (`grub.rs`), UEFI load option and BootOrder encoding (`loadopt.rs`), PE/path and loader classification (`detection.rs`) |
| bootapp-efi | IA32/X64/AArch64 firmware: volume discovery, volume selectors, menu, key handling, volume-bound chainload, BootOrder registration (key `F`) |
| bootapp-bios | i686 freestanding Rust menu, disk scan, own-disk entries; NASM bootstrap and BIOS mode-transition thunk |
| bootapp-cli | Windows host tools: validate, preview, grub-check, read-only scan, staging, `install`/`uninstall` (UEFI ESP directory, BIOS MBR disk image or disk), `bootorder` |

## Core modules

- `lib.rs`: `Config`, `Entry`, `VolumeSelector`, `MenuState`, path validators, `TIMEOUT_DISABLED`.
- `grub.rs`: parses the GRUB subset to the same `Config` model. Nothing is executed. Unknown
  commands are rejected with a line number. Harmless commands are accepted with a warning.
- `loadopt.rs`: byte-level `Boot####`, `BootOrder` and device-path helpers shared by the firmware
  manager and the CLI. No firmware access.
- `detection.rs`: loader classification (unchanged in v0.3; see [DETECTION.md](DETECTION.md)).

## UEFI

Config precedence on the manager's own volume: `bootapp.cfg`, else `grub.cfg`, else
auto-only (see [SPEC.md](../SPEC.md)). Auto-discovered loaders are always appended after the
configured entries.

Configured entries carry a `VolumeSelector`:

- `Own` entries resolve on the volume the manager was loaded from.
- `File`, `FsUuid` and `Label` entries (from GRUB `search`) are resolved **lazily**, when the
  user or the timer boots them. Startup does not probe slow or failing media. An entry whose volume
  is absent stays listed and fails with a visible "volume not found" message.

Discovery enumerates every firmware-visible SimpleFileSystem volume and keeps the volume handle
for each candidate. The volume's full device path plus the file path is passed to `LoadImage`.
PE architecture and EFI application subsystem are checked before automatic entries are offered.
Firmware loading remains the final authority.

Volume identity helpers (`volume_policy.rs`) compare FAT serials (`XXXX-XXXX`, read from the boot
sector) and labels case-insensitively, and check self-recursion after resolution. The DevicePath and
BlockIO protocols are opened non-exclusively; see [SPEC.md](../SPEC.md#firmware-uefi).

Key `F` writes BootApp's own `Boot####` and `BootOrder` through `bootorder.rs` (reuse of an entry
that already describes this file, otherwise the first free number; no deletion). Trace markers
(`BOOTAPP_*` on the serial port) record configuration source, menu state, selected volume, start
and return status, and BootOrder changes.

Discovery identifies loader hints, not proof that an OS is installed or intact.
APFS/HFS+ need firmware-provided filesystem support; no custom driver is included.
See [detection contract](DETECTION.md).

## Legacy BIOS

A 512-byte stage1 loads a bounded stage2 (120 sectors at LBA1). Rust runs in protected mode and
uses assembly thunks for BIOS disk, keyboard, time and final real-mode handoff. Disk reads never
write sectors. Stage2 first checks CPUID for FXSR, SSE and SSE2 and halts with
`BOOTAPP_BIOS_CPU_UNSUPPORTED` if any is missing. The protected-mode stack top is
min(0x90000, EBDA base or INT 12h size), 16-byte aligned.

The manager skips its own drive, scans up to eight BIOS EDD disks, and chainloads the selected
entry at 0000:7c00 with its actual DL and DS:SI.

**Own-disk entries (v0.3).** When BootApp is installed into an MBR disk's post-MBR gap by
`bootapp install bios`, the manager lists that disk's own entries:

- primary partitions 1..4 whose VBR is 55AA-signed, with a usable type and a start LBA ≥ 128;
- `Saved original MBR` (LBA121), when it is 55AA-signed, not blank, and differs from the current LBA0.
  It boots with the current partition table patched in.

Other drives are listed as before: their MBR is chainloaded.
The installer layout is in [INSTALL.md](INSTALL.md#22-layout-after-installation). Memory map,
ABI and the menu rules are in [crates/bootapp-bios/README.md](../crates/bootapp-bios/README.md).

**Real-mode addressing.** Real-mode code addresses its data relative to the image base with DS/CS
set to 0x1000, never with 32-bit linear offsets above 0xFFFF. `scripts/check-bios-realmode.py`
is a static lint for this rule. QEMU TCG does not enforce real-mode segment limits, so the
functional tests alone did not catch the v0.2 bug.

The manager does not require GRUB to start. A selected Linux disk can use its own GRUB as its OS
loader. EFI-only GPT Windows is not BIOS bootable.

## Installer and BootOrder (host CLI)

- `install uefi` / `uninstall uefi` write only an ESP directory tree (dry-run by default). An
  install manifest (`EFI\BOOTAPP\install-manifest.txt`) journals every change so uninstall can undo
  exactly that set.
- `install bios` / `uninstall bios` write only the target MBR disk image or `\\.\PhysicalDriveN`
  (elevated for a physical drive, even on dry run). The backup file (`BOOTAPP_BIOS_BACKUP 1`,
  125,440 bytes: header, original and installed LBA0..=121) is a new file written before the first
  write. Uninstall verifies each sector against the installed or original bytes unless `--force`.
- UEFI install writes ESP files through temp, fsync and no-replace rename. The manifest allow-list
  limits what uninstall may touch.
- `bootorder list|set|next` reads and writes firmware variables through Windows APIs (elevated, dry-run
  by default).

Raw disk access is in `disk.rs`; firmware variable access in `nvram.rs`; Windows API wrappers in
`win.rs`; the SHA-256 used for manifest fingerprints in `sha256.rs`.

## Validation boundary

Unit tests, EFI fixtures, QEMU image tests, installed OS boot, and physical hardware acceptance are
separate layers. VM tests use generated image files and private copies of NVRAM, never host disks
or host firmware. See [VALIDATION.md](VALIDATION.md).
