# Installing BootApp (v0.3)

This guide covers `bootapp install` and `bootapp uninstall` for UEFI and Legacy BIOS,
plus `bootapp bootorder` for firmware boot order on Windows.

**Read this first.**

- Every write command is a **dry run unless `--apply` is given**. A dry run prints the exact plan
  (files, sectors, firmware variables with hex payloads) and changes nothing.
- Installing writes to an EFI System Partition directory (UEFI), to a disk's first sectors (BIOS),
  or to firmware variables (`--register`, `bootorder`). A mistake can stop a machine from booting.
  Test on an image or a spare disk first, and keep a recovery path (see [Recovery](#recovery)).
- The installer and BootOrder code were verified only on generated disk images, ESP directories,
  and QEMU/OVMF copies of NVRAM. They have **not** been run on physical hardware in this project.
  Secure Boot is not covered by this guide (see [SECURE-BOOT.md](SECURE-BOOT.md)).
- Privileges:
  - Image files and ESP directories: no administrator rights needed, on any OS.
  - `\\.\PhysicalDriveN` (Windows only): elevated prompt required, **even for a dry run**, because
    the sectors must be read to plan.
  - `mountvol`, `--register`, `uninstall uefi` of a registered install, and `bootorder` (including
    `list`): elevated prompt required (firmware-variable APIs).

Build the inputs first:

```powershell
rustup target add x86_64-unknown-uefi
cargo build --release -p bootapp-efi --target x86_64-unknown-uefi
cargo build --release -p bootapp-cli
# BIOS only (needs NASM and the i686 target; see README.md):
python scripts/build-bios.py --nasm C:/path/to/nasm.exe --output out/bios
```

Outputs used below: `target/x86_64-unknown-uefi/release/bootapp-efi.efi` (UEFI manager),
`target/release/bootapp.exe` (CLI), and `out/bios/bootapp-bios.img` (BIOS image).

Argument rules that apply to every command: a flag value may not start with `--` (a missing value is
an error, not the next flag). On Windows a bare drive such as `S:` means the volume root `S:\`.

---

## 1. UEFI

### 1.1 What gets written

Given `--esp <ESP root>`, `--arch <x86|x64|aarch64>` and `--manager <FILE>`:

| Path (relative to ESP root) | Content |
|---|---|
| `EFI\BOOTAPP\BOOTAPPX64.EFI` (`BOOTAPPIA32.EFI`, `BOOTAPPAA64.EFI` for other arches) | the manager, after PE/architecture validation |
| `EFI\BOOTAPP\bootapp.cfg` or `EFI\BOOTAPP\grub.cfg` | only with `--config`; the name is chosen from the input file name (`grub.cfg` → GRUB subset) |
| `EFI\BOOT\BOOTX64.EFI` (other arch names likewise) | only with `--fallback`; the existing file is backed up first |
| `EFI\BOOTAPP\backup\BOOTX64.EFI` | the original fallback loader, only when `--fallback` replaces one |
| `EFI\BOOTAPP\install-manifest.txt` | the change record used by `uninstall uefi` |
| NVRAM `Boot####` + `BootOrder` | only with `--register` (Windows host, elevated) |

Options:

- `--config FILE`: validated first. Invalid configuration is an error. A configuration with no
  entries for the chosen architecture only produces a warning; auto-discovered loaders are still listed.
- If `EFI\BOOTAPP\bootapp.cfg` already exists and you install a `grub.cfg`, the CLI warns that
  `bootapp.cfg` takes precedence.
- `--fallback`: installs the manager as the removable-media loader `EFI\BOOT\BOOTX64.EFI`. This
  replaces whatever vendor loader was there; the original is kept under `EFI\BOOTAPP\backup\`.
- `--register`: creates or reuses one `Boot####` (description `BootApp`, file path of this manager)
  and puts it first in `BootOrder`. If `BootOrder` does not exist yet, it is created as the BootApp
  number followed by every existing `Boot####` in ascending order, so other entries stay bootable.
  Requires elevation and `--apply`.

### 1.2 Step by step

1. Open an elevated PowerShell and mount the ESP (only needed when the ESP is not already visible):

   ```powershell
   mountvol S: /s
   ```

   This mounts the system ESP at `S:\`. Pass the ESP root (`S:\`), not a subfolder. Unmount with
   `mountvol S: /d` when finished.

2. Dry run. Read the plan; nothing is written:

   ```powershell
   target\release\bootapp.exe install uefi --esp S:\ --arch x64 `
     --manager target\x86_64-unknown-uefi\release\bootapp-efi.efi `
     --config examples\bootapp.cfg
   ```

3. Apply the same command with `--apply`. Add `--fallback` only if you want the removable-media path
   too, and `--register` only if you want a firmware boot entry:

   ```powershell
   target\release\bootapp.exe install uefi --esp S:\ --arch x64 `
     --manager target\x86_64-unknown-uefi\release\bootapp-efi.efi `
     --config examples\bootapp.cfg --apply
   ```

4. Check that `EFI\BOOTAPP\install-manifest.txt` reads `state=complete`. Reboot and use the firmware
   boot menu to choose BootApp if it is not first.

5. Optional: `bootorder list` (elevated) shows the firmware order. Inside the BootApp menu, `F` also
   moves BootApp to the first place.

### 1.3 Safety rules (UEFI)

- **Atomic writes.** Each file is written to a unique temporary name in its directory, flushed,
  then renamed into place. New files and backups use a no-replace rename, so a file that appears
  after planning is never overwritten. A replacement re-checks the planned original just before the
  rename.
- **No reparse points.** Every path component under `--esp` must be an ordinary directory or file.
  Symbolic links, junctions and other reparse points are refused.
- A file that already exists with **different** content is refused unless it is replaced with a
  backup (`--fallback`). BootApp-owned files with identical content are kept and not removed later.
- An existing manifest means "installed": an identical re-install is a no-op. Any other change is
  refused until you run `uninstall uefi`.
- The manifest is written first with `state=pending` as a journal and rewritten with `state=complete`
  at the end. If an apply step fails, the installer rolls back using the journal and reports what it did.
- Only the ESP root you passed is written. No other disk or firmware setting is changed except
  the explicit `--register` variables.

### 1.4 Uninstall

```powershell
target\release\bootapp.exe uninstall uefi --esp S:\          # dry run
target\release\bootapp.exe uninstall uefi --esp S:\ --apply
```

Uninstall reads the manifest and undoes what it records:

- removes the files and directories the install created;
- restores each `replaced=` file from its backup (`EFI\BOOTAPP\backup\...`);
- keeps files that were already identical (`kept=`);
- for a `boot=` entry with `created`, deletes the `Boot####` and its `BootOrder` slot;
- for a `boot=` entry with `reused`, puts back the **prior payload** of that `Boot####` and its
  **prior BootOrder position** recorded in the manifest, instead of deleting it.

**Files modified after installation.** If a managed file changed after install (for example
Windows servicing rewrote `EFI\BOOT\BOOTX64.EFI`), uninstall does **not** touch that file. It notes
it, undoes everything else, **keeps the manifest** and its backups, and exits with a non-zero status
and a summary. Resolve the items it reports and run `uninstall uefi` again, or review them and remove
the manifest and leftover backups by hand.

### 1.5 Manifest format

`EFI\BOOTAPP\install-manifest.txt` is a text file of `key=value` lines. Fields inside a line are
separated by `|`:

```text
manifest_version=1
bootapp_version=0.3
arch=x64
state=pending|complete
dir=<rel>                                   directory created by install
file=<rel>|<size>|<sha256>                  file created by install
kept=<rel>|<size>|<sha256>                  identical file that already existed
replaced=<rel>|<size>|<sha256>|<backup rel>|<orig size>|<orig sha256>
boot=<XXXX>|created|<file path>|<device path hex>|-|-
boot=<XXXX>|reused|<file path>|<device path hex>|<prior payload hex>|<prior BootOrder index or ->
```

- Uninstall accepts **only** the paths the installer itself can produce: `EFI\BOOTAPP\BOOTAPP<ARCH>.EFI`,
  `EFI\BOOT\BOOT<ARCH>.EFI`, `EFI\BOOTAPP\backup\BOOT<ARCH>.EFI`, `EFI\BOOTAPP\bootapp.cfg`,
  `EFI\BOOTAPP\grub.cfg`, and the directories `EFI`, `EFI\BOOTAPP`, `EFI\BOOTAPP\backup`, `EFI\BOOT`
  (case-insensitive). **Any other line rejects the whole manifest** and nothing is changed.
- Do not edit the manifest by hand. An unknown manifest version or a malformed line is an error.

---

## 2. Legacy BIOS (MBR disk)

BIOS installation writes BootApp into the **post-MBR gap** of an MBR disk and rewrites the first
sector's boot code. Use a disk image first.

### 2.1 Preconditions (all must hold, or the install is refused)

- The target is an **MBR** disk: LBA0 is signed `55AA`, and no partition has type `0xEE`.
- Every partition starts at or after **LBA 128**.
- **LBA1 through LBA121 are all zero.** If any byte is non-zero, the install refuses and names the
  first non-zero sector. This is typically a GRUB `core.img` already in the gap.
- The `--bios-image` is a BootApp BIOS image: its LBA0 is `55AA`-signed and carries the BootApp stage1
  signature (`2APB`) at offset 440. Any other file is refused.
- The target is a regular image file, or `\\.\PhysicalDriveN` on Windows (elevated).
- The backup path is a **new** file (it must not exist).
- The system-disk guard applies (see 2.4).

### 2.2 Layout after installation

| Sectors | Content |
|---|---|
| LBA0 bytes 0..440 | BootApp stage1 code |
| LBA0 bytes 440..510 | **preserved** from the target: disk signature and partition table |
| LBA0 bytes 510..512 | `55 AA` |
| LBA1..=120 | BootApp stage2 (from the image) |
| LBA121 | full copy of the **original** LBA0 |

Because LBA0[440..510] is preserved, you can edit the partition table later without re-installing.

### 2.3 Step by step (image first)

1. Build `out/bios/bootapp-bios.img` (see the top of this guide).
2. Dry run against a **copy** of the disk image (the target must already exist). Nothing is written:

   ```powershell
   target\release\bootapp.exe install bios --target disk.img `
     --bios-image out\bios\bootapp-bios.img --backup disk.backup.bin
   ```

3. Apply. The backup is written, flushed and read back before the first target write:

   ```powershell
   target\release\bootapp.exe install bios --target disk.img `
     --bios-image out\bios\bootapp-bios.img --backup disk.backup.bin --apply
   ```

   **Backup format** `BOOTAPP_BIOS_BACKUP 1`, **125,440 bytes**: a 512-byte text header containing
   `original_sha256=` and `installed_sha256=`, then the **original** LBA0..=121 (122 sectors), then the
   **installed** LBA0..=121 exactly as written. Keep the backup on a different disk or medium.

4. Physical disk (elevated; the dry run also needs elevation):

   ```powershell
   target\release\bootapp.exe install bios --target \\.\PhysicalDrive2 `
     --bios-image out\bios\bootapp-bios.img --backup D:\safe-place\disk2.backup.bin
   ```

   **Check the disk number twice.** A wrong number overwrites that disk's MBR boot code and
   post-MBR gap.

**Write order.** The installer writes LBA1..=120 (stage2) first, then LBA121 (copy of the original
LBA0), and **LBA0 last**. Until LBA0 changes, the disk still boots its original MBR. If a write fails
before LBA0 is changed, the message says so, and `uninstall bios` with the same backup cleans up the
partially written gap. Once LBA0 is changed, the message says LBA0 state is unknown; restore with
`uninstall bios` as the message states.

### 2.4 System disk and refusals

- The CLI refuses a target that holds the running Windows volume (found with
  `GetSystemWindowsDirectoryW`) or the firmware boot/system partition (from
  `HKLM\SYSTEM\Setup\SystemPartition`), unless you pass `--allow-system-disk`. Do not pass it unless
  you intend to modify that disk. If `SystemPartition` is not set, only the Windows volume's disk is
  protected and a warning is printed.
- The installer refuses, with no change, when:
  - the target LBA0 is not a 55AA-signed MBR (unpartitioned or GPT);
  - a partition has type `0xEE`, or starts before LBA 128;
  - LBA1..=121 is not all zero;
  - the BIOS image is not a BootApp image (signature or size);
  - the backup file already exists;
  - the disk is already BootApp-installed (repeat install);
  - the target is the system disk and `--allow-system-disk` was not given.

### 2.5 Uninstall

```powershell
target\release\bootapp.exe uninstall bios --target disk.img --backup disk.backup.bin          # dry run
target\release\bootapp.exe uninstall bios --target disk.img --backup disk.backup.bin --apply
```

Uninstall restores LBA0..=121 from the backup. It writes LBA0 first (the original boot code, so the
disk boots its original MBR again), then LBA1..=121. It keeps the **current** LBA0[440..510], so
partition-table edits made after installation survive.

**Safety check.** Uninstall refuses if any sector holds neither BootApp's installed bytes nor the
original bytes recorded in the backup. This protects a boot loader installed later (for example
GRUB `core.img` in the gap). The refusal lists the sectors that differ.

`--force` overrides that check. It is **DANGEROUS**: it overwrites whatever is there now, including
a later boot loader. Use it only after you have inspected the disk and accept losing that data.

### 2.6 Boot behaviour

With BootApp installed on a disk, the BIOS menu lists for that disk:

- each primary partition 1..4 whose VBR is 55AA-signed, chainloaded with its partition entry;
- `Saved original MBR`, only when LBA121 is 55AA-signed, is not blank (its boot code is not all
  zero), and differs from the current boot code. It boots with the **current** partition table
  patched into the saved sector, so partition edits after installation still apply.

A blank saved sector is not listed. Other BIOS disks are shown as before. See
[crates/bootapp-bios/README.md](../crates/bootapp-bios/README.md) for the menu rules and memory map.
Extended partitions and GPT disks are not booted through the BIOS path.

**CPU requirement.** BootApp's BIOS stage2 checks CPUID for FXSR, SSE and SSE2 at start-up. A CPU
without them is refused with `BOOTAPP_BIOS_CPU_UNSUPPORTED` and the machine halts; it does not fault
later.

---

## 3. Windows BootOrder (`bootorder`)

```powershell
target\release\bootapp.exe bootorder list                 # read-only
target\release\bootapp.exe bootorder set 0003,0001        # dry run
target\release\bootapp.exe bootorder set 0003,0001 --apply
target\release\bootapp.exe bootorder next 0003 --apply    # BootNext for one boot
```

Run these from an elevated PowerShell (the firmware-variable APIs require it, even for `list`).
`set` replaces the whole order with the numbers given, so check `list` first. The commands never
delete `Boot####` variables.

---

## Recovery

- **Keep the backup or manifest.** The UEFI manifest and `EFI\BOOTAPP\backup\` let `uninstall uefi`
  undo the change. The BIOS backup file is the only record of the original LBA0..=121 and of the
  installed bytes; store it off the target disk.
- **Interrupted UEFI install.** Rerun `uninstall uefi` on the ESP; it uses the `state=pending` journal.
- **Interrupted BIOS install.** If LBA0 was not yet changed, the disk still boots its original MBR;
  run `uninstall bios` with the same backup. If LBA0 state is unknown, run `uninstall bios` as the
  error message says.
- **If firmware will not start BootApp**, use the firmware boot menu (vendor key at power-on) and
  choose the original Windows Boot Manager or Linux loader. Then uninstall or correct the install.
- **BitLocker**: have the recovery key saved before changing the ESP or boot firmware variables.
  Changing boot components can trigger BitLocker recovery.
- **Keep a rescue medium** (Windows recovery USB or Linux live USB) before any change.
- **BIOS rescue**: boot a rescue medium and restore LBA0..=121 from the backup, or run
  `uninstall bios` from a machine that can run `bootapp.exe`.
- If a failed `--register` leaves a `Boot####` behind, `uninstall uefi` removes or restores it when the
  manifest records it. The tool never deletes variables it did not create.

## See also

- [SPEC.md](../SPEC.md) for the configuration format.
- [GRUB subset](GRUB.md) for `grub.cfg`.
- [Secure Boot guide](SECURE-BOOT.md).
- [Validation](VALIDATION.md) for what was tested and what is NOT_RUN.
