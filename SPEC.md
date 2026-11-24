# BootApp core and configuration contract (v0.3)

The core/config API is retained from v0.1 and extended in v0.3 (GRUB subset, volume
selectors, BootOrder encoding). BIOS and cross-volume discovery details are in
docs/ARCHITECTURE.md and docs/DETECTION.md. Installer behaviour is in docs/INSTALL.md.

## Core API (crate bootapp-core, no_std + alloc)

- `Architecture::{X86,X64,Aarch64}`; `Architecture::parse(&str)->Option<Self>`;
  `as_str()->&'static str`.
- `VolumeSelector::{Own, File(String), FsUuid(String), Label(String)}`, default `Own`.
  Selects the firmware volume that an entry's `path` is resolved on:
  - `Own`: the boot manager's own volume (every `bootapp.cfg` entry).
  - `File(path)`: the first volume, in discovery order, on which this absolute backslash path
    exists (GRUB `search --file`).
  - `FsUuid("XXXX-XXXX")`: FAT volume serial, compared case-insensitively (GRUB `search --fs-uuid`).
  - `Label(text)`: filesystem volume label, compared case-insensitively (GRUB `search --label`).
- `Entry { id:String, title:String, path:String, arch:Option<Architecture>, options:String,
  volume:VolumeSelector }`.
- `Config { timeout:u32, default:String, entries:Vec<Entry> }`;
  `Config::parse(&str)->Result<Self,ConfigError>`; `entries_for(Architecture)->Vec<&Entry>`;
  `default_index(Architecture)->Option<usize>`.
- `ConfigError { line:usize, message:&'static str }`, implements Display (`line 0` = document error).
- `TIMEOUT_DISABLED: u32 = u32::MAX`: "wait for a key forever" (GRUB `set timeout=-1`).
  `bootapp.cfg` itself still accepts only 0..=3600.
- `validate_efi_path(&str)->bool`: absolute backslash path, no empty/`.`/`..` components,
  no trailing space or dot in a component, no colon, slash, `* ? " < > |` or control characters,
  final component longer than `.efi` and ending in `.efi` (case-insensitive).
- `validate_volume_path(&str)->bool`: same path rules, but **no extension requirement**. Used for
  GRUB `chainloader` targets and Linux EFI-stub kernels and initrds such as `\vmlinuz-6.12`.
  The bootapp.cfg path rule (`validate_efi_path`) is unchanged.

### Modules

- `grub` (see [docs/GRUB.md](docs/GRUB.md)): `GRUB_CONFIG_PATH = "\EFI\BOOTAPP\grub.cfg"`;
  `parse(&str)->Result<GrubConfig{config:Config, warnings:Vec<GrubWarning>}, GrubError>`;
  `MAX_SUBMENU_DEPTH = 8`. Nothing is executed. Unsupported constructs are rejected with a line
  number; commands with no effect (for example `insmod`, `terminal_*`, `echo`, `sleep`) are
  accepted with a warning.
- `loadopt`: UEFI `EFI_LOAD_OPTION` encode/decode (`LoadOption{attributes, description,
  device_path, optional_data}`), `BootOrder` encode/decode, `boot_variable_name`,
  `parse_boot_number`, `prepend_unique`, `remove_number`, `first_free_boot_number`,
  `HardDriveNode`, `hard_drive_file_device_path`, `append_file_path`, `device_path_file_name`,
  `describe_device_path`, `same_boot_target`. Constants: `EFI_GLOBAL_VARIABLE_GUID`,
  `BOOT_VARIABLE_ATTRIBUTES` (`0x7`), `LOAD_OPTION_ACTIVE`, `BOOTAPP_DESCRIPTION = "BootApp"`.
  Pure byte-level helpers shared by the firmware manager and the CLI.
- `detection`: loader classification (unchanged in v0.3; see docs/DETECTION.md).

## Configuration: bootapp.cfg (UTF-8, maximum 64 KiB)

Line-oriented INI subset, comments start with `#` or `;` at the beginning of a trimmed line.
Global keys `timeout=5` (0..3600), `default=windows`; section `[entry windows]`.
Entry keys `title=Windows`, `path=\EFI\Microsoft\Boot\bootmgfw.efi`, `arch=x64` (optional
x86/x64/aarch64), `options=` (optional UTF-8, converted to UTF-16 load options). No inline
comments, interpolation, quoting or script execution.

Reject unknown or duplicate keys, duplicate IDs, invalid numbers or architectures, missing
title or path, empty entries, unknown default. Defaults: timeout=5, first entry ID.
IDs: ASCII alphanumeric plus `-` and `_`, 1..64 characters. Maximum 128 entries.
Entries from bootapp.cfg always have `volume = Own`.

## Configuration: grub.cfg (GRUB subset)

Read only when `\EFI\BOOTAPP\bootapp.cfg` is absent (see Config precedence). Same size and entry
limits. Summary (full rules in docs/GRUB.md):

- `menuentry` and `submenu` (nesting up to 8), `set timeout=N|-1`, `set default=`.
  `timeout` and `default` are accepted only outside submenu bodies; inside a submenu they are
  ignored with a warning. `default=` may name a raw grub-mkconfig `--id` (kept beside the BootApp id).
- `chainloader P` → `Entry.path = P`, options `""`.
- `linux P args...` (`linuxefi` also accepted) with `initrd A [B...]` (`initrdefi` also):
  `path = P`, `options = "args... initrd=\A initrd=\B"`. Kernel arguments containing whitespace
  or `"` are quoted GRUB-style (`"` and `\` escaped inside the quotes). A later `initrd` replaces
  the earlier list. Linux EFI-stub reads `initrd=` relative to the kernel's volume, so every path
  must be on the entry's volume.
- `search -f|--file P`, `-u|--fs-uuid X`, `-l|--label X` with `--set[=root]`: sets the volume
  selector used by `$root` / `($root)` path prefixes. No search means `Own`.
- GRUB `/a/b` paths become `\a\b`. Explicit device names such as `(hd0,gpt1)` are rejected.
- `set root=$var` copies the search selector of `$var`, so `root` still names a search result.
  A non-empty literal `root` is an error at the point where it is used; `search` is required.
- Submenu bodies are scoped: entries and variables set inside a submenu do not leak out.
- Not supported: script execution, `if`/`for`/`while`, functions, `source`/`configfile`,
  normal mode. Each is rejected with a line number. A volume is usable only if the firmware can
  read it: `search --fs-uuid` matches FAT volume serials only, and ext4/btrfs are not supported.

## Config precedence (UEFI manager)

On the manager's own volume:

1. `\EFI\BOOTAPP\bootapp.cfg` if present;
2. else `\EFI\BOOTAPP\grub.cfg` if present;
3. else auto-discovery only.

A present but invalid file is a visible error and is never silently ignored. Configured entries
come first; auto-discovered loaders are appended after them in every case. The file that is
actually read is traced as `BOOTAPP_CONFIG_SOURCE`.

## Menu state

`MenuState::new(len:usize, default_index:usize, timeout:u32)->Self`;
`selected()->usize`; `remaining()->Option<u32>` (`None` when the countdown is disabled);
`handle(MenuEvent)->MenuAction`. Events: `Up`, `Down`, `Tick`, `Enter`, `CancelTimeout`.
Actions: `None`, `Boot(usize)`. Any key cancels the automatic countdown; `Tick` emits `Boot` at
zero once; an empty menu never boots; `Up`/`Down` wrap safely. `TIMEOUT_DISABLED` starts with
no countdown.

## Firmware (UEFI)

- Text console. `Up`/`Down` select, `Enter` boots, any key stops the countdown (`Esc` also
  cancels it), `R` reboots, `Q` exits to firmware.
- `F` (menu key): registers this manager as the first firmware boot option. It creates or reuses
  one `Boot####` (description `BootApp`, file path of this manager) and prepends it to `BootOrder`
  using `loadopt::prepend_unique`. It never deletes variables. The result is reported on screen and
  traced as `BOOTAPP_BOOTORDER_SET`.
- Entries with `VolumeSelector::File|FsUuid|Label` are resolved lazily when booted. An unresolvable
  selector stays listed and fails with "volume not found"; the menu remains usable.
- Self-recursion protection applies after resolution: the manager's own executable path on its
  own volume is rejected.
- Firmware `LoadImage` and `StartImage` enforce PE architecture and security policy. Load/start
  failure returns to the menu with the countdown cancelled. The watchdog stays disabled while the
  menu is shown.
- UTF-16 load options are passed through `LoadedImage` and kept allocated until `StartImage`
  returns.
- Firmware DevicePath and BlockIO protocols are opened non-exclusively (`GET_PROTOCOL`) for
  read-only use, so probing a volume does not disconnect the firmware's partition or filesystem
  drivers. The SimpleFileSystem and LoadedImage opens stay exclusive, as the source comments explain.
- No raw disk writes. Firmware variables are written only by the `F` key (`Boot####`, `BootOrder`)
  and by the CLI with `--register` / `bootorder`, both as described in docs/INSTALL.md.

## Installer contracts (CLI)

- `install uefi` / `uninstall uefi`: manifest `EFI\BOOTAPP\install-manifest.txt`, format in
  docs/INSTALL.md section 1.5. Uninstall accepts only the paths install can produce (allow-list);
  any other line rejects the whole manifest. `boot=` lines are `created` (delete on uninstall) or
  `reused` (restore the recorded prior payload and BootOrder position). Files modified after install
  are left in place and reported; everything else is undone, the manifest is kept, and the exit
  status is non-zero. Files are written via temp + fsync + rename; reparse-point components under
  `--esp` are refused.
- `install bios` / `uninstall bios`: on-disk layout:
  LBA0[0..440] = stage1, LBA0[440..510] preserved, LBA0[510..512] = `55AA`, LBA1..=120 = stage2,
  LBA121 = full copy of the original LBA0. Target must be an MBR disk, every partition at or after
  LBA 128, LBA1..=121 all zero. The image must carry the BootApp stage1 signature (`2APB`, offset 440).
  Install writes LBA1..=120, then LBA121, then LBA0 last.
- BIOS backup `BOOTAPP_BIOS_BACKUP 1`, 125,440 bytes: 512-byte header (`original_sha256=`,
  `installed_sha256=`), original LBA0..=121, installed LBA0..=121. Written, flushed and verified before
  the first target write; it must be a new file.
- `uninstall bios` restores LBA0 first, then LBA1..=121, keeping the current LBA0[440..510]. It refuses
  if any sector holds neither the installed nor the original bytes; `--force` overrides (DANGEROUS).
- System-disk guard: refuses a target holding the running Windows volume (`GetSystemWindowsDirectoryW`)
  or the firmware boot/system partition (`HKLM\SYSTEM\Setup\SystemPartition`) unless
  `--allow-system-disk`.
- Privileges: image files and ESP directories need no administrator rights. A physical drive path
  (`\\.\PhysicalDriveN`) needs elevation even for a dry run. `--register`, `bootorder` (including
  `list`) and uninstall of a registered UEFI install use firmware-variable APIs and need elevation.
- CLI argument rule: a flag value may not start with `--`. On Windows a bare `S:` means `S:\`.

## Packaging and compatibility

- Rust target names: `i686-unknown-uefi`, `x86_64-unknown-uefi`, `aarch64-unknown-uefi`.
- Removable paths: `EFI/BOOT/BOOTIA32.EFI`, `BOOTX64.EFI`, `BOOTAA64.EFI`.
- Installed manager names: `EFI\BOOTAPP\BOOTAPPIA32.EFI`, `BOOTAPPX64.EFI`, `BOOTAPPAA64.EFI`.
- Unsigned builds require a firmware policy permitting them; no automatic key change.
- Firmware architecture must match the executable, independently of CPU capability.
- BIOS support is a separate loader and is not implied by x86 UEFI support.
- BIOS stage2 requires CPUID FXSR, SSE and SSE2 and halts with `BOOTAPP_BIOS_CPU_UNSUPPORTED`
  otherwise. Protected-mode stack top = min(0x90000, EBDA base or INT 12h size), 16-byte aligned.
- User-visible version string: `BootApp 0.3`.
