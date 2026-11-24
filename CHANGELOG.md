# Changelog

All notable changes to BootApp are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses semantic versioning for its user-visible version string (`BootApp 0.3`).

## [0.3.0] - Unreleased

### Fixed

- BIOS stage2 real-mode addressing. Real-mode code used 32-bit linear offsets above 0xFFFF, which
  raises a general protection fault on real CPUs and under KVM/WHPX. QEMU TCG does not enforce
  real-mode segment limits, so the v0.2 functional tests passed. Data is now addressed relative to
  the image base. A static regression lint, `scripts/check-bios-realmode.py`, guards the rule.
- BIOS CPU requirement made explicit. Stage2 checks CPUID for FXSR, SSE and SSE2 at start-up and
  halts with `BOOTAPP_BIOS_CPU_UNSUPPORTED` otherwise. The v0.2 "Pentium III" QEMU evidence was
  not valid for SSE2 (TCG ignores CPUID); v0.3 tests use `-cpu pentium3,+sse2`, and a `no-sse2`
  case checks the refusal.
- BIOS stack placement: the protected-mode stack top is min(0x90000, EBDA base or INT 12h size),
  16-byte aligned, instead of a fixed address.
- BIOS saved original MBR: it boots with the current partition table patched into the saved sector,
  so partition edits after installation still apply. A blank saved sector (all-zero boot code) is
  not listed.
- BIOS countdown: the on-screen remaining time is live (5, 4, 3, 2, 1 seconds) and its text was
  corrected. Any key pauses it with `Automatic boot paused.`.
- UEFI serial trace protocol is opened non-exclusively (`GET_PROTOCOL`). An exclusive open
  disconnected the firmware console driver, which hid the boot menu under OVMF during v0.2 development.
- UEFI DevicePath and BlockIO protocols are opened non-exclusively for volume probing, so firmware
  partition and filesystem drivers are not disconnected.
- GRUB `set root=$var` copies the search selector of `$var`. A non-empty literal `root` is reported
  as an error where it is used.
- GRUB `timeout` and `default` inside a submenu body are ignored with a warning; submenu bodies are
  scoped, so variables set inside them do not leak out.
- GRUB kernel arguments that contain whitespace or `"` are quoted GRUB-style in the UEFI load options.
- GRUB a later `initrd` replaces the earlier initrd list.
- Version strings report `0.3` in the EFI manager banner, BIOS banner, CLI help and installer
  manifest. (Crate `Cargo.toml` versions still read `0.2.0`; see the validation notes.)

### Added

- GRUB `grub.cfg` compatibility subset, documented in [docs/GRUB.md](docs/GRUB.md):
  `menuentry`, `submenu`, `chainloader`, `linux`/`linuxefi` with `initrd`/`initrdefi`,
  `search --file|--fs-uuid|--label` with `--set`, `set timeout|default`, `timeout=-1`.
  `default=` may name a raw grub-mkconfig `--id`. Nothing is executed. Unsupported commands are
  rejected with a line number; harmless commands (for example `insmod`) are accepted with a warning.
- `grub.cfg` precedence on the EFI manager's volume: `bootapp.cfg`, then `grub.cfg`, then
  auto-discovery. Auto-discovered loaders always follow configured entries.
- Volume selectors (`VolumeSelector::{Own, File, FsUuid, Label}`) for GRUB-derived entries,
  resolved lazily when the entry is booted. An unresolvable selector stays listed and fails with a
  visible message.
- `TIMEOUT_DISABLED` for `timeout=-1` (wait for a key forever).
- `validate_volume_path` for GRUB chainloader and Linux EFI-stub paths (no `.efi` requirement).
- Menu key `F`: registers BootApp as the first firmware boot option (`Boot####` + `BootOrder`).
  Never deletes variables. If `BootOrder` does not exist, it is created as the BootApp number followed
  by the existing `Boot####` entries in ascending order.
- Host CLI commands:
  - `bootapp grub-check <grub.cfg> [arch]`;
  - `bootapp install uefi` / `uninstall uefi` into an ESP directory (dry run by default, `--apply`,
    `--fallback`, `--config`, `--register`, manifest journal);
  - `bootapp install bios` / `uninstall bios` into an MBR disk image or `\\.\PhysicalDriveN`
    (dry run by default, mandatory new backup file, `--force` for uninstall only);
  - `bootapp bootorder list|set|next` (Windows, elevated, dry run by default).
- UEFI manifest `boot=` records for `created` and `reused` `Boot####` entries, including the prior
  payload and prior BootOrder position of a reused entry, so uninstall can restore it.
- BIOS backup format `BOOTAPP_BIOS_BACKUP 1` (125,440 bytes): header with SHA-256 values, original
  LBA0..=121 and installed LBA0..=121.
- BIOS own-disk entries after installation: each primary partition with a valid VBR, and
  `Saved original MBR` when it is not blank. Other disks are unchanged.
- BIOS install layout: stage1 in LBA0, preserved LBA0[440..510], stage2 in LBA1..=120, original
  LBA0 copy in LBA121. The installer writes LBA0 last, so an interrupted install is restorable.
- Installer dry-run plans print exact files, sectors and firmware-variable payloads.
- Secure Boot guide ([docs/SECURE-BOOT.md](docs/SECURE-BOOT.md)) and `scripts/sign-efi.py`
  (throwaway keys, `sbsign`, `sbverify`; no keys are enrolled automatically).
- Test scripts: `test-grub.py`, `test-bootorder.py`, `test-uefi-install.py` (with
  `--existing-fallback`), `test-bios-install.py`, `check-bios-realmode.py`, and `test-bios.py` cases
  `auto`, `keyboard`, `empty`, `own-partition`, `saved-mbr`, `saved-mbr-blank`, `no-sse2`. Every
  script accepts `--out` and `--qemu-dir`.
- GitHub Actions workflow [`.github/workflows/ci.yml`](.github/workflows/ci.yml): Rust tests,
  clippy, UEFI builds for three targets, BIOS image build, real-mode lint, BIOS and UEFI QEMU
  tests, installer tests, Secure Boot signing check. The workflow has not yet run on GitHub.
- Documents: [docs/INSTALL.md](docs/INSTALL.md), [docs/GRUB.md](docs/GRUB.md),
  [docs/SECURE-BOOT.md](docs/SECURE-BOOT.md), this changelog.

### Changed

- `uninstall uefi` skips a managed file that changed after install (for example rewritten by Windows
  servicing), reports it, undoes everything else, keeps the manifest and its backups, and exits
  with a non-zero status. Re-run after resolving the reported items.
- `uninstall bios` refuses when any sector holds neither the installed nor the original bytes
  (for example a GRUB `core.img` installed later). `--force` overrides this and is dangerous.
- BIOS install writes LBA1..=120, then LBA121, then LBA0 last.
- A flag value may not start with `--`. On Windows a bare `S:` is normalised to `S:\`.
- Image-file targets for `install bios` / `uninstall bios` need no administrator rights on any OS.
  `\\.\PhysicalDriveN` needs elevation even for a dry run.
- The README no longer says that there is no installer, no BootOrder management and no GRUB
  subset. It describes the documented subset, the dry-run installer, the BootOrder commands and what
  has and has not been run.
- Configuration in `SPEC.md` gained the GRUB subset, volume selectors, `TIMEOUT_DISABLED`, the
  precedence rules and the menu keys.
- `docs/ARCHITECTURE.md` and `docs/DETECTION.md` describe the new core modules (`grub`, `loadopt`),
  EFI volume and BootOrder modules, and BIOS own-disk detection.
- `docs/VALIDATION.md` is organized by script and case, with the v0.2 real-OS results kept and
  labelled as not re-run for v0.3.
- `validate` selects the GRUB subset when the file is named `grub.cfg`.

### Security

- Every installer and BootOrder write is a dry run unless `--apply` is given.
- UEFI manifest allow-list: uninstall accepts only the paths install can create. Any other line
  rejects the whole manifest, so a tampered manifest cannot delete or restore arbitrary files.
- UEFI writes use a unique temporary file, fsync and rename. New files and backups use a no-replace
  rename; replacements re-check the planned original just before the rename. Symbolic links, junctions
  and other reparse points in the ESP path are refused.
- Installers never overwrite a file without a backup. BIOS install writes and verifies a new backup
  of LBA0..=121 before the first target write, refuses a non-empty post-MBR gap, and requires the
  BootApp stage1 signature in the image.
- BIOS uninstall verifies every sector against the installed or original bytes before writing, so a
  later boot loader is not destroyed unless `--force` is given.
- System-disk guard uses Windows APIs: the disk holding the running Windows volume
  (`GetSystemWindowsDirectoryW`) and the disk named by `HKLM\SYSTEM\Setup\SystemPartition` are
  refused unless `--allow-system-disk` is given.
- `Boot####` deletion is limited to entries the install itself created; reused entries are restored
  to their recorded prior state.
- Firmware volume probing uses non-exclusive protocol opens. The EFI manager never writes raw disks.
- Config and file reads are bounded (64 KiB configuration, 64 MiB manager image, bounded load option
  and path sizes). Malformed configuration is a visible error, not silently ignored.
- Secure Boot helper keys are throwaway and are written without passphrases. They must not be used
  on real hardware. No key enrollment is automated.

## [0.2.0]

Release date not recorded in the repository.

### Added

- Legacy BIOS backend: an independent assembly bootstrap and a Rust freestanding menu. It scans
  up to eight BIOS EDD disks, shows GRUB and Windows NTFS loader evidence, and chainloads the
  selected disk's MBR with its real `DL`.
- UEFI backend for IA32, x64 and AArch64: firmware filesystem discovery across all visible volumes,
  automatic loader candidates for Windows, Linux (shim, GRUB, systemd-boot, UKI), macOS and the
  removable-media fallback paths, PE architecture and subsystem checks.
- Volume-bound chainload: every UEFI entry keeps the volume it was found on.
- macOS path classification (CoreServices, Apple EFI, Preboot UUID, Recovery). Reading depends on
  firmware-provided filesystem support; there is no APFS or HFS+ driver.
- Windows x64 `bootapp.exe` tool: `validate`, `preview`, read-only `scan` of mounted directories
  (reparse points excluded), and `stage` into a new directory.
- Configuration format `bootapp.cfg` (INI subset, 64 KiB limit, 128 entries) and menu state
  with a timeout that any key cancels.
- Test scripts for BIOS and UEFI QEMU fixtures, auto-discovery, and the OS lab runner.

### Validation

- 32 unit and regression tests in the v0.2 run.
- Debian 13.7 (kernel 6.12.113) booted under BIOS and UEFI in QEMU; Windows Server 2022 Evaluation
  booted under BIOS and Windows Server 2025 Evaluation under UEFI in QEMU. Both reached the desktop.
  The v0.2 BIOS QEMU runs used a Pentium III-class CPU model and are not evidence of SSE2 support.
- Not run in v0.2: macOS actual OS boot, physical hardware, Secure Boot, hosted CI.

### Known limits (v0.2)

- No GRUB script execution; no GRUB installer; no BootOrder management; no MBR or ESP writes.
- The BIOS build contained a real-mode addressing bug that QEMU TCG did not expose (fixed in 0.3).
