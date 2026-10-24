# Auto-discovery interface

New core module `bootapp_core::detection` (no changes to existing Config/Entry API).
Public APIs:
- `OsKind::{Windows,Linux,MacOs,Unknown}`, `label(self)->&'static str`.
- `classify_loader_path(path:&str)->Option<OsKind>` accepts validated absolute EFI
  paths, case-insensitive. bootmgfw.efi = Windows; grub*/shim*/systemd-boot*.efi =
  Linux; paths under EFI/Linux with .efi = Linux UKI; exact
  System/Library/CoreServices/boot.efi or EFI/APPLE/.../boot.efi = MacOs.
  Also recognize root canonical UUID/System/Library/CoreServices/boot.efi
  (firmware-visible APFS Preboot) and com.apple.recovery.boot/boot.efi.
  EFI/BOOT/BOOT*.EFI = Unknown; no arbitrary file named boot.efi classification.
- `pe_architecture(bytes:&[u8])->Option<Architecture>` bounded PE header reads,
  MZ, PE signature, consistent machine/PE32(+), EFI application subsystem.
- `loader_candidates(arch:Architecture)->Vec<(&'static str,OsKind)>`: common
  architecture-specific Windows, Ubuntu/Debian/Fedora/openSUSE, systemd, Linux UKI,
  macOS and fallback full paths. Discovery should also enumerate EFI vendor dirs
  for arbitrary recognized basenames and EFI/Linux UKIs.
- `classify_legacy_boot_sector(bytes:&[u8])->Option<OsKind>` sector 0x55aa required,
  recognizable Windows NTFS + BOOTMGR/NTLDR strings, GRUB string Linux, otherwise
  Unknown. Classification is evidence about a loader, not proof an OS is installed.

Firmware target binds an owned Entry to the volume Handle. Config entries retain
current-volume behavior. Auto entries validate candidate PE header/machine, skip
manager itself by volume+path and deduplicate volume+normalized path. Prefer explicit
config default when present; otherwise deterministic auto order. Missing config
is normal and enters auto mode; malformed existing config remains a visible error.
Do not silently call matching filenames proof of a successful OS boot.
