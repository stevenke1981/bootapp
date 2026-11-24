# Auto-discovery and volume selection contract (v0.3)

Public APIs in `bootapp_core::detection` are unchanged from v0.2. v0.3 adds volume selection
for GRUB-derived entries (UEFI) and own-disk entries (BIOS).

## Core classification (`bootapp_core::detection`)

- `OsKind::{Windows,Linux,MacOs,Unknown}`, `label(self)->&'static str`.
- `classify_loader_path(path:&str)->Option<OsKind>` accepts validated absolute EFI paths,
  case-insensitive. `bootmgfw.efi` = Windows; `grub*`, `shim*`, `systemd-boot*.efi` = Linux;
  paths under `EFI/Linux` with `.efi` = Linux UKI; exact `System/Library/CoreServices/boot.efi`
  or `EFI/APPLE/.../boot.efi` = MacOs. Also recognized: the root canonical UUID
  `System/Library/CoreServices/boot.efi` (APFS Preboot) and `com.apple.recovery.boot/boot.efi`.
  `EFI/BOOT/BOOT*.EFI` = Unknown. No arbitrary file named `boot.efi` is classified.
- `pe_architecture(bytes:&[u8])->Option<Architecture>`: bounded PE header reads. MZ, PE signature,
  consistent machine and PE32/PE32+, EFI application subsystem.
- `loader_candidates(arch:Architecture)->Vec<(&'static str,OsKind)>`: common architecture-specific
  Windows, Ubuntu/Debian/Fedora/openSUSE, systemd, Linux UKI, macOS and fallback paths. Discovery
  also enumerates EFI vendor directories for arbitrary recognized basenames and `EFI/Linux` UKIs.
- `classify_legacy_boot_sector(bytes:&[u8])->Option<OsKind>`: sector `55AA` required; recognizable
  Windows NTFS plus `BOOTMGR`/`NTLDR` strings = Windows; GRUB string = Linux; otherwise Unknown.

Classification is evidence about a loader, not proof that an OS is installed or will boot.

## UEFI discovery and selection

- Automatic entries are discovered on every firmware-visible SimpleFileSystem volume and keep
  the original volume handle. Candidates must pass the PE header and architecture check. The
  manager's own image (same volume and path) is skipped, and volume + normalized path is
  de-duplicated.
- Configured entries come first. Automatic entries follow in deterministic order, in every
  configuration case (see [SPEC.md](../SPEC.md#config-precedence-uefi-manager)).
- A missing config is normal: automatic mode. A present but malformed config is a visible error.

### Volume selectors (GRUB `search`)

GRUB `search --file|--fs-uuid|--label ... --set[=root]` followed by `chainloader` or
`linux`/`initrd` produces an entry whose `VolumeSelector` names the volume to use:

| Selector | Matches |
|---|---|
| `File(path)` | first volume, in discovery order, where that absolute path exists |
| `FsUuid("XXXX-XXXX")` | FAT volume serial read from the boot sector (FAT only; case-insensitive) |
| `Label(text)` | volume label, case-insensitive |

- Resolution is lazy: the volume is probed when the entry is booted, not at startup.
- An unresolvable selector stays listed and reports "volume not found". The menu keeps working.
- Every path of one `menuentry` must be on the same volume, because Linux resolves `initrd=` on
  the kernel's volume.
- Explicit GRUB device names such as `(hd0,gpt1)` are rejected. Use `search`.

## BIOS discovery

Own-disk discovery applies only when the manager's `DL` is a hard disk that BootApp was installed
on (install layout in [INSTALL.md](INSTALL.md#22-layout-after-installation)):

- Each primary partition 1..4 is listed if its type is usable (not empty, extended `05`/`0F`/`85`,
  or GPT protective `EE`), its start LBA is at least 128, and its VBR is 55AA-signed. Booting it
  copies the 16-byte partition entry to `0000:0620` and passes `DS:SI` and `DL`.
- `Saved original MBR` is listed when LBA121 is 55AA-signed, its boot code (bytes 0..440) is not all
  zero, and it differs from the current LBA0 boot code. It boots that sector with the **current**
  partition table patched in, so partition edits after installation still apply. A blank saved sector
  is not listed.
- LBA0 of the own disk is never offered (it is BootApp itself).

Other BIOS disks are scanned as before, by their MBR. The label `Unknown` is used when the boot code
is not recognized; a sector is not claimed to contain an OS unless the evidence above says so.

Do not treat a matching filename or signature as proof of a successful OS boot.
