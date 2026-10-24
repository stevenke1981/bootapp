# Architecture

BootApp is a Rust boot manager with independent BIOS and UEFI frontends. It
chainloads existing OS loaders; it is not a GRUB script interpreter or a complete
native Linux/Multiboot kernel loader.

| Component | Responsibility |
|---|---|
| bootapp-core | no_std + alloc config parser, menu state, PE/path and loader classification |
| bootapp-efi | IA32/X64/AArch64 firmware filesystem discovery, keyboard/timer menu and volume-bound EFI chainload |
| bootapp-bios | i686 freestanding Rust menu and disk scan; minimal assembly bootstrap and BIOS mode-transition thunk |
| bootapp-cli | Windows host config validation, preview, read-only mounted-volume scan and new-directory staging |

## UEFI

The loaded image's volume owns explicit configuration entries. Automatic entries
are discovered on every firmware-visible SimpleFileSystem volume and retain the
original volume Handle. A full volume device path plus selected file path is
passed to LoadImage. PE architecture and EFI application subsystem are checked
before offering automatic entries; firmware loading remains the final authority.

Missing config enables discovery. Existing malformed config remains a visible
error. Keys cancel automatic timeout; load/start errors return to a paused menu.
UTF-16 load options remain allocated through StartImage and cleanup.

Discovery identifies loader hints, not proof that an OS is installed or intact.
APFS/HFS+ need firmware-provided filesystem support; no custom driver is included.
See [detection contract](DETECTION.md).

## Legacy BIOS

A 512-byte bootstrap loads a bounded stage2. Rust runs in protected mode and
uses assembly thunks for BIOS disk, keyboard, time and final real-mode handoff.
Disk reads never write sectors. It skips its own drive, scans up to eight BIOS
EDD disks, and chainloads the selected disk's MBR at 0000:7c00 with its actual DL.
The target MBR owns subsequent active/extended partition and OS-loader handling.

The manager does not require GRUB to start. A selected Linux disk can naturally
use its existing GRUB as its OS loader. EFI-only GPT Windows is not BIOS bootable.
See [memory map and build procedure](../crates/bootapp-bios/README.md).

## Validation boundary

Unit tests, EFI fixtures, installed OS boot, and physical hardware acceptance are
separate layers. VM tests use file images and private overlays, never host disks.
The project has no installer that writes host MBR/ESP, NVRAM or security keys.
