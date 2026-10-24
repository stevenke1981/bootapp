# BootApp core and configuration contract

The core/config API is retained from v0.1. BIOS and cross-volume discovery
extensions are described in docs/ARCHITECTURE.md and docs/DETECTION.md.

## Core API (crate bootapp-core, no_std + alloc, no dependencies)
`Architecture::{X86,X64,Aarch64}`; `Architecture::parse(&str)->Option<Self>`;
`as_str()->&'static str`. `Entry { id:String, title:String, path:String,
arch:Option<Architecture>, options:String }`. `Config { timeout:u32,
default:String, entries:Vec<Entry> }`; `Config::parse(&str)->Result<Self,ConfigError>`;
`entries_for(Architecture)->Vec<&Entry>`; `default_index(Architecture)->Option<usize>`.
ConfigError implements Display. `validate_efi_path(&str)->bool` accepts only
absolute backslash EFI paths with no empty/dot/dotdot components, colon, slash,
control characters or embedded NUL; extension .efi case-insensitive.

## Configuration (UTF-8, maximum 64 KiB)
Line-oriented INI subset, comments start with # or ; at beginning of trimmed line.
Global keys `timeout=5` (0..3600), `default=windows`; section `[entry windows]`.
Entry keys `title=Windows`, `path=\EFI\Microsoft\Boot\bootmgfw.efi`,
`arch=x64` (optional x86/x64/aarch64), `options=` (optional UTF-8, converted to UTF-16
load options). No inline comments, interpolation, quoting or GRUB script execution.
Reject unknown/duplicate keys, duplicate IDs, invalid numbers/architectures, missing
title/path, empty entries, unknown default. Defaults: timeout=5, first entry ID.
IDs: ASCII alphanumeric plus '-' and '_', 1..64 characters. Max 128 entries.

## Menu state
`MenuState::new(len:usize,default_index:usize,timeout:u32)->Self`;
`selected()->usize`, `remaining()->Option<u32>`;
`handle(MenuEvent)->MenuAction`. Events: Up, Down, Tick, Enter, CancelTimeout.
Actions: None, Boot(usize). Every key interaction cancels automatic countdown;
Tick emits Boot at zero once; empty menu never boots; Up/Down wrap safely.

## Firmware
UEFI text console with Up/Down/Enter, Esc to cancel timeout, R reboot, Q exit.
Read `\EFI\BOOTAPP\bootapp.cfg` on the loaded image's filesystem volume only.
FileDevicePath points to the selected file on that same volume; reject the
manager's own executable path to prevent obvious recursion. Firmware LoadImage
and StartImage enforce PE architecture/security policy. Preserve useful error
status; leave watchdog disabled while showing menu. Pass UTF-16 options through
LoadedImage. Keep backing options allocation live until StartImage returns.
Load failure returns to menu with timeout cancelled. No raw disk writes.

## Packaging and compatibility
Rust target names: i686-unknown-uefi, x86_64-unknown-uefi, aarch64-unknown-uefi.
Removable paths: EFI/BOOT/BOOTIA32.EFI, BOOTX64.EFI, BOOTAA64.EFI.
Unsigned builds require a firmware policy permitting them; no automatic key change.
Firmware architecture must match the executable, independently of CPU capability.
BIOS support requires a separate loader and is not implied by x86 UEFI support.
