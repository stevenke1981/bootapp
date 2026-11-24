//! BIOS on-disk install into the MBR post-MBR gap (layout per BRIEF/SPEC):
//!
//! * target: MBR disk, LBA0 signed 55AA, no 0xEE partition, every partition
//!   start >= LBA 128, LBA1..=121 all zero;
//! * after install: LBA0[0..440] = BootApp stage1, LBA0[440..510] preserved
//!   (disk signature + partition table), LBA0[510..512] = 55AA,
//!   LBA1..=120 = BootApp stage2, LBA121 = copy of the ORIGINAL LBA0;
//! * a host backup file is written (new file, fsynced and verified) before the
//!   first target write. Format (`BOOTAPP_BIOS_BACKUP 1`, 125440 bytes):
//!   512-byte text header (`original_sha256=`, `installed_sha256=`), then the
//!   ORIGINAL LBA0..=121, then the INSTALLED LBA0..=121 exactly as written;
//! * uninstall restores LBA0..=121 from the backup, keeping the current
//!   LBA0[440..510] so later partition-table edits survive. It refuses unless
//!   every sector still holds either BootApp's installed bytes or the original
//!   bytes (so a boot loader installed later, e.g. GRUB core.img, is never
//!   destroyed); a partially applied install (gap written, LBA0 untouched) is
//!   therefore restorable. `--force` overrides that check and is DANGEROUS.

use crate::{
    disk::{SECTOR, SectorIo},
    sha256,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

/// LBA0..=121.
pub const REGION_SECTORS: usize = 122;
pub const REGION_BYTES: usize = REGION_SECTORS * SECTOR;
/// Header sector + original region + installed region.
pub const BACKUP_FILE_BYTES: usize = SECTOR + 2 * REGION_BYTES;
/// Image LBA0 (stage1) + LBA1..=120 (stage2).
pub const IMAGE_SECTORS: usize = 121;
pub const SAVED_MBR_LBA: usize = 121;
pub const MIN_PARTITION_LBA: u32 = 128;
/// Signature dword the BootApp stage1 carries at LBA0 offset 440 ("2APB").
pub const STAGE1_SIGNATURE: u32 = 0x4250_4132;
const CODE_BYTES: usize = 440;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const BACKUP_MAGIC: &str = "BOOTAPP_BIOS_BACKUP 1\n";

fn signed(sector: &[u8]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xAA
}

/// Validate a BootApp BIOS image (bootapp-bios.img) and return its first 121 sectors.
pub fn validate_image(image: &[u8]) -> Result<&[u8], String> {
    if image.len() < IMAGE_SECTORS * SECTOR {
        return Err(format!(
            "BIOS image is {} bytes; at least {} sectors ({} bytes) are required",
            image.len(),
            IMAGE_SECTORS,
            IMAGE_SECTORS * SECTOR
        ));
    }
    if !signed(&image[..SECTOR]) {
        return Err("BIOS image LBA0 is not 55AA-signed".into());
    }
    if u32::from_le_bytes([image[440], image[441], image[442], image[443]]) != STAGE1_SIGNATURE {
        return Err(format!(
            "BIOS image LBA0 lacks the BootApp stage1 signature 0x{STAGE1_SIGNATURE:08X} at offset 440; not a bootapp-bios.img"
        ));
    }
    if image[..CODE_BYTES].iter().all(|&b| b == 0) {
        return Err("BIOS image stage1 code area is empty".into());
    }
    if image[SECTOR..IMAGE_SECTORS * SECTOR]
        .iter()
        .all(|&b| b == 0)
    {
        return Err("BIOS image stage2 (LBA1..=120) is empty".into());
    }
    Ok(&image[..IMAGE_SECTORS * SECTOR])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub index: usize,
    pub active: bool,
    pub kind: u8,
    pub start: u32,
    pub sectors: u32,
}

pub fn partitions(mbr: &[u8]) -> Vec<Partition> {
    (0..4)
        .filter_map(|index| {
            let entry = &mbr[446 + 16 * index..462 + 16 * index];
            let kind = entry[4];
            (kind != 0).then(|| Partition {
                index: index + 1,
                active: entry[0] == 0x80,
                kind,
                start: u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]),
                sectors: u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]),
            })
        })
        .collect()
}

/// Check the current LBA0..=121 of a target against every install precondition.
pub fn check_target(region: &[u8], image: &[u8], disk_sectors: u64) -> Result<(), String> {
    if region.len() != REGION_BYTES {
        return Err("internal: region must be LBA0..=121".into());
    }
    if disk_sectors < u64::from(MIN_PARTITION_LBA) {
        return Err(format!(
            "target has {disk_sectors} sectors; at least {MIN_PARTITION_LBA} are required"
        ));
    }
    let mbr = &region[..SECTOR];
    if !signed(mbr) {
        return Err(
            "target LBA0 is not a 55AA-signed MBR (unpartitioned or not MBR); refusing".into(),
        );
    }
    if mbr[..CODE_BYTES] == image[..CODE_BYTES] {
        return Err(
            "BootApp BIOS stage1 is already installed on this target; run `bootapp uninstall bios` first"
                .into(),
        );
    }
    let parts = partitions(mbr);
    if parts.iter().any(|p| p.kind == 0xEE) {
        return Err(
            "target has a GPT protective MBR (partition type 0xEE); BIOS MBR install supports MBR disks only"
                .into(),
        );
    }
    for p in &parts {
        if p.start < MIN_PARTITION_LBA {
            return Err(format!(
                "partition {} starts at LBA {} (< {MIN_PARTITION_LBA}); the post-MBR gap is too small",
                p.index, p.start
            ));
        }
        if u64::from(p.start) >= disk_sectors {
            return Err(format!(
                "partition {} starts at LBA {} beyond the end of the target ({disk_sectors} sectors)",
                p.index, p.start
            ));
        }
    }
    if let Some(offset) = region[SECTOR..].iter().position(|&b| b != 0) {
        return Err(format!(
            "post-MBR gap LBA1..=121 is in use (first non-zero byte in LBA {}), e.g. by GRUB core.img; refusing",
            1 + offset / SECTOR
        ));
    }
    Ok(())
}

/// The exact new LBA0..=121 bytes for an install (preconditions already checked).
pub fn build_install(original: &[u8], image: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; REGION_BYTES];
    out[..CODE_BYTES].copy_from_slice(&image[..CODE_BYTES]);
    out[CODE_BYTES..510].copy_from_slice(&original[CODE_BYTES..510]);
    out[510] = 0x55;
    out[511] = 0xAA;
    out[SECTOR..IMAGE_SECTORS * SECTOR].copy_from_slice(&image[SECTOR..IMAGE_SECTORS * SECTOR]);
    out[SAVED_MBR_LBA * SECTOR..].copy_from_slice(&original[..SECTOR]);
    out
}

/// Backup file bytes: header sector, original region, installed region.
pub fn encode_backup(original: &[u8], installed: &[u8]) -> Vec<u8> {
    let header = format!(
        "{BACKUP_MAGIC}original_sha256={}\ninstalled_sha256={}\n",
        sha256::hex_digest(original),
        sha256::hex_digest(installed)
    );
    let mut out = vec![0u8; SECTOR];
    out[..header.len()].copy_from_slice(header.as_bytes());
    out.extend_from_slice(original);
    out.extend_from_slice(installed);
    out
}

/// Parse and verify a backup file; returns (original, installed) regions.
pub fn decode_backup(file: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if file.len() != BACKUP_FILE_BYTES {
        return Err(format!(
            "backup is {} bytes; a BootApp {} backup is exactly {BACKUP_FILE_BYTES} bytes",
            file.len(),
            BACKUP_MAGIC.trim()
        ));
    }
    let header_end = file[..SECTOR]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(SECTOR);
    let header = std::str::from_utf8(&file[..header_end])
        .map_err(|_| "backup header is not text".to_string())?;
    let body = header
        .strip_prefix(BACKUP_MAGIC)
        .ok_or("not a BootApp BIOS backup (missing BOOTAPP_BIOS_BACKUP 1 header)")?;
    let (original, installed) = file[SECTOR..].split_at(REGION_BYTES);
    let expected = format!(
        "original_sha256={}\ninstalled_sha256={}\n",
        sha256::hex_digest(original),
        sha256::hex_digest(installed)
    );
    if body != expected || file[header_end..SECTOR].iter().any(|&b| b != 0) {
        return Err(
            "backup header checksums do not match its contents (corrupted or edited)".into(),
        );
    }
    if !signed(&original[..SECTOR]) {
        return Err("backup original LBA0 is not 55AA-signed".into());
    }
    if original[SECTOR..].iter().any(|&b| b != 0) {
        return Err(
            "backup original LBA1..=121 is not all zero; not a BootApp pre-install backup".into(),
        );
    }
    if installed[SAVED_MBR_LBA * SECTOR..] != original[..SECTOR] {
        return Err("backup installed LBA121 does not match its original LBA0".into());
    }
    Ok((original, installed))
}

pub enum Uninstall {
    AlreadyRestored,
    Restore(Vec<u8>),
}

fn describe_lbas(lbas: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < lbas.len() {
        let start = lbas[index];
        let mut end = start;
        while index + 1 < lbas.len() && lbas[index + 1] == end + 1 {
            index += 1;
            end = lbas[index];
        }
        parts.push(if start == end {
            format!("{start}")
        } else {
            format!("{start}..={end}")
        });
        index += 1;
    }
    parts.join(",")
}

/// Compute the restored LBA0..=121 from the current target region and the backup.
/// Every current sector must hold either BootApp's installed bytes or the
/// original bytes, unless `force`.
pub fn build_uninstall(
    current: &[u8],
    backup_file: &[u8],
    force: bool,
) -> Result<Uninstall, String> {
    let (original, installed) = decode_backup(backup_file)?;
    let mut restored = original.to_vec();
    restored[CODE_BYTES..510].copy_from_slice(&current[CODE_BYTES..510]);
    if current == restored.as_slice() {
        return Ok(Uninstall::AlreadyRestored);
    }
    let mut foreign = Vec::new();
    let code = &current[..CODE_BYTES];
    if code != &installed[..CODE_BYTES] && code != &original[..CODE_BYTES] {
        foreign.push("LBA0 boot code (bytes 0..440)".to_string());
    }
    if !signed(&current[..SECTOR]) {
        foreign.push("LBA0 55AA signature".to_string());
    }
    let lbas: Vec<usize> = (1..REGION_SECTORS)
        .filter(|&lba| {
            let range = lba * SECTOR..(lba + 1) * SECTOR;
            current[range.clone()] != installed[range.clone()]
                && current[range.clone()] != original[range]
        })
        .collect();
    if !lbas.is_empty() {
        foreign.push(format!("LBA {}", describe_lbas(&lbas)));
    }
    if !foreign.is_empty() {
        let what = foreign.join(", ");
        if !force {
            return Err(format!(
                "target {what} holds neither BootApp's installed bytes nor the original bytes from this backup (another boot loader such as GRUB may have been installed since); refusing. --force restores the backup anyway (DANGEROUS: destroys whatever is there now)"
            ));
        }
        eprintln!("WARNING: --force: overwriting foreign content in {what}.");
    }
    Ok(Uninstall::Restore(restored))
}

fn read_region(disk: &mut dyn SectorIo) -> Result<Vec<u8>, String> {
    if disk.sector_count() < REGION_SECTORS as u64 {
        return Err(format!(
            "target has only {} sectors; at least {REGION_SECTORS} are required",
            disk.sector_count()
        ));
    }
    let mut region = vec![0u8; REGION_BYTES];
    disk.read_sectors(0, &mut region)?;
    Ok(region)
}

fn describe_target(region: &[u8], sectors: u64) {
    println!("  target size: {sectors} sectors");
    for p in partitions(&region[..SECTOR]) {
        println!(
            "  partition {}: type 0x{:02X}{} start LBA {} sectors {}",
            p.index,
            p.kind,
            if p.active { " active" } else { "" },
            p.start,
            p.sectors
        );
    }
}

/// Write the backup as a NEW file, fsync it and read it back.
fn write_backup(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("cannot create NEW backup file {}: {e}", path.display()))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("writing backup {} failed: {e}", path.display()))?;
    drop(file);
    let check = fs::read(path).map_err(|e| format!("cannot re-read backup: {e}"))?;
    if check != data {
        return Err(format!(
            "backup {} failed read-back verification",
            path.display()
        ));
    }
    Ok(())
}

pub fn read_image_file(path: &Path) -> Result<Vec<u8>, String> {
    crate::read_bounded(path, MAX_IMAGE_BYTES)
}

fn verify(disk: &mut dyn SectorIo, expected: &[u8]) -> Result<(), String> {
    let mut check = vec![0u8; expected.len()];
    disk.read_sectors(0, &mut check)?;
    if check != expected {
        return Err("read-back verification of LBA0..=121 failed".into());
    }
    Ok(())
}

/// Install (dry-run unless `apply`).
pub fn install(
    disk: &mut dyn SectorIo,
    image: &[u8],
    backup_path: &Path,
    apply: bool,
) -> Result<(), String> {
    let image = validate_image(image)?;
    let original = read_region(disk)?;
    describe_target(&original, disk.sector_count());
    check_target(&original, image, disk.sector_count())?;
    if fs::symlink_metadata(backup_path).is_ok() {
        return Err(format!(
            "backup file {} already exists; the backup must be a NEW file",
            backup_path.display()
        ));
    }
    let new = build_install(&original, image);
    let backup = encode_backup(&original, &new);
    println!("Plan (BIOS MBR install):");
    println!(
        "  1. create backup {} ({BACKUP_FILE_BYTES} bytes: header + original LBA0..=121 sha256 {} + installed LBA0..=121), fsync, verify",
        backup_path.display(),
        sha256::hex_digest(&original)
    );
    println!(
        "  2. write LBA1..=120 = BootApp stage2 ({} bytes), then LBA121 = copy of original LBA0",
        120 * SECTOR
    );
    println!(
        "  3. write LBA0[0..440] = BootApp stage1 code; LBA0[440..510] (disk signature + partition table) preserved; LBA0[510..512] = 55AA"
    );
    println!(
        "  4. flush and verify; resulting LBA0..=121 sha256 {}",
        sha256::hex_digest(&new)
    );
    if !apply {
        println!("DRY RUN: nothing was written. Re-run with --apply to perform this plan.");
        return Ok(());
    }
    write_backup(backup_path, &backup)?;
    println!(
        "done: backup written and verified: {}",
        backup_path.display()
    );
    // Gap first, LBA0 last: until LBA0 changes the disk still boots as before.
    let tail = disk
        .write_sectors(1, &new[SECTOR..SAVED_MBR_LBA * SECTOR])
        .and_then(|_| disk.write_sectors(SAVED_MBR_LBA as u64, &new[SAVED_MBR_LBA * SECTOR..]))
        .and_then(|_| disk.flush());
    if let Err(e) = tail {
        return Err(format!(
            "{e}. LBA0 was NOT modified (the disk still boots its original MBR); LBA1..=121 may be partially written. Clean up with `bootapp uninstall bios --target <target> --backup {} --apply`",
            backup_path.display()
        ));
    }
    println!("done: wrote LBA1..=121");
    disk.write_sectors(0, &new[..SECTOR])
        .and_then(|_| disk.flush())
        .and_then(|_| verify(disk, &new))
        .map_err(|e| {
            format!(
                "{e}. LBA1..=121 were written; LBA0 state unknown. Restore with `bootapp uninstall bios --target <target> --backup {} --apply`",
                backup_path.display()
            )
        })?;
    println!("done: wrote LBA0 and verified LBA0..=121. BootApp BIOS installed.");
    Ok(())
}

/// Uninstall (dry-run unless `apply`). `force` skips the "only BootApp or
/// original bytes on disk" check (DANGEROUS).
pub fn uninstall(
    disk: &mut dyn SectorIo,
    backup: &[u8],
    force: bool,
    apply: bool,
) -> Result<(), String> {
    let current = read_region(disk)?;
    describe_target(&current, disk.sector_count());
    let restored = match build_uninstall(&current, backup, force)? {
        Uninstall::AlreadyRestored => {
            println!("Target LBA0..=121 already matches the backup; nothing to do.");
            return Ok(());
        }
        Uninstall::Restore(restored) => restored,
    };
    let lba0_changes = current[..SECTOR] != restored[..SECTOR];
    println!("Plan (BIOS MBR uninstall):");
    if lba0_changes {
        println!(
            "  1. write LBA0 = original boot code from the backup (keeping current LBA0[440..510] partition table)"
        );
    } else {
        println!(
            "  1. LBA0 already holds the original boot code (BootApp was never activated); unchanged"
        );
    }
    println!("  2. write LBA1..=121 = original (all zero)");
    println!(
        "  3. flush and verify; resulting LBA0..=121 sha256 {}",
        sha256::hex_digest(&restored)
    );
    if !apply {
        println!("DRY RUN: nothing was written. Re-run with --apply to perform this plan.");
        return Ok(());
    }
    // LBA0 first: once it is restored the disk boots its original MBR again.
    if lba0_changes {
        disk.write_sectors(0, &restored[..SECTOR])
            .and_then(|_| disk.flush())
            .map_err(|e| format!("{e}. Nothing else was changed; retry the uninstall"))?;
        println!("done: restored LBA0");
    }
    disk.write_sectors(1, &restored[SECTOR..])
        .and_then(|_| disk.flush())
        .and_then(|_| verify(disk, &restored))
        .map_err(|e| {
            format!(
                "{e}. LBA0 holds the original boot code; retry the uninstall to clear LBA1..=121"
            )
        })?;
    println!(
        "done: restored LBA1..=121 and verified. The backup file was kept; delete it when no longer needed."
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct MemDisk {
        pub data: Vec<u8>,
        pub fail_writes_at: Option<u64>,
    }

    impl SectorIo for MemDisk {
        fn sector_count(&self) -> u64 {
            (self.data.len() / SECTOR) as u64
        }
        fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), String> {
            let start = lba as usize * SECTOR;
            buffer.copy_from_slice(
                self.data
                    .get(start..start + buffer.len())
                    .ok_or("read out of range")?,
            );
            Ok(())
        }
        fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
            if self.fail_writes_at == Some(lba) {
                return Err("injected write failure".into());
            }
            let start = lba as usize * SECTOR;
            self.data
                .get_mut(start..start + data.len())
                .ok_or("write out of range")?
                .copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    pub fn image() -> Vec<u8> {
        let mut image = vec![0u8; 1024 * 1024];
        for (i, b) in image[..440].iter_mut().enumerate() {
            *b = (i % 251) as u8 + 1;
        }
        image[440..444].copy_from_slice(&0x42504132u32.to_le_bytes());
        image[510] = 0x55;
        image[511] = 0xAA;
        for (i, b) in image[512..121 * 512].iter_mut().enumerate() {
            *b = (i % 253) as u8 ^ 0x5A;
        }
        image
    }

    pub fn disk(start: u32, kind: u8) -> MemDisk {
        let mut data = vec![0u8; 16 * 1024 * 1024];
        data[..440].fill(0xC3);
        data[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        let entry = &mut data[446..462];
        entry[0] = 0x80;
        entry[4] = kind;
        entry[8..12].copy_from_slice(&start.to_le_bytes());
        entry[12..16].copy_from_slice(&(32768 - start).to_le_bytes());
        data[510] = 0x55;
        data[511] = 0xAA;
        data[start as usize * 512] = 0xEB;
        MemDisk {
            data,
            fail_writes_at: None,
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bootapp-bios-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn exact_install_bytes_and_round_trip() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        let original = target.data.clone();
        let backup = temp("roundtrip.bak");
        install(&mut target, &image, &backup, false).unwrap();
        assert_eq!(target.data, original, "dry run must not write");
        assert!(!backup.exists(), "dry run must not create the backup");
        install(&mut target, &image, &backup, true).unwrap();
        let d = &target.data;
        assert_eq!(&d[..440], &image[..440]);
        assert_eq!(&d[440..510], &original[440..510]);
        assert_eq!(&d[510..512], &[0x55, 0xAA]);
        assert_eq!(&d[512..121 * 512], &image[512..121 * 512]);
        assert_eq!(&d[121 * 512..122 * 512], &original[..512]);
        assert_eq!(&d[122 * 512..], &original[122 * 512..]);
        let saved = fs::read(&backup).unwrap();
        let (saved_original, saved_installed) = decode_backup(&saved).unwrap();
        assert_eq!(saved_original, &original[..REGION_BYTES]);
        assert_eq!(saved_installed, &target.data[..REGION_BYTES]);
        // Already installed → refused.
        let second = temp("second.bak");
        let err = install(&mut target, &image, &second, true).unwrap_err();
        assert!(err.contains("already installed"), "{err}");
        // Uninstall round trip.
        let installed = target.data.clone();
        uninstall(&mut target, &saved, false, false).unwrap();
        assert_eq!(target.data, installed, "dry run must not write");
        uninstall(&mut target, &saved, false, true).unwrap();
        assert_eq!(target.data, original);
        // Idempotent second uninstall.
        uninstall(&mut target, &saved, false, true).unwrap();
        assert_eq!(target.data, original);
        fs::remove_file(backup).unwrap();
    }

    #[test]
    fn uninstall_keeps_current_partition_table() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        let original = target.data.clone();
        let backup = temp("table.bak");
        install(&mut target, &image, &backup, true).unwrap();
        target.data[446 + 16 + 4] = 0x83; // user added partition 2 afterwards
        target.data[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&20000u32.to_le_bytes());
        let saved = fs::read(&backup).unwrap();
        uninstall(&mut target, &saved, false, true).unwrap();
        assert_eq!(&target.data[..440], &original[..440]);
        assert_eq!(target.data[446 + 16 + 4], 0x83);
        assert!(target.data[512..122 * 512].iter().all(|&b| b == 0));
        fs::remove_file(backup).unwrap();
    }

    #[test]
    fn refusals() {
        let image = image();
        let cases: Vec<(MemDisk, &str)> = vec![
            (disk(2048, 0xEE), "GPT protective"),
            (disk(127, 0x0C), "starts at LBA 127"),
            (
                {
                    let mut d = disk(2048, 0x0C);
                    d.data[60 * 512 + 7] = 1;
                    d
                },
                "post-MBR gap",
            ),
            (
                {
                    let mut d = disk(2048, 0x0C);
                    d.data[121 * 512] = 1;
                    d
                },
                "post-MBR gap",
            ),
            (
                {
                    let mut d = disk(2048, 0x0C);
                    d.data[511] = 0;
                    d
                },
                "not a 55AA-signed MBR",
            ),
            (
                {
                    let mut d = disk(2048, 0x0C);
                    d.data[..440].copy_from_slice(&image[..440]);
                    d
                },
                "already installed",
            ),
        ];
        for (index, (mut target, expected)) in cases.into_iter().enumerate() {
            let before = target.data.clone();
            let backup = temp(&format!("refuse-{index}.bak"));
            let err = install(&mut target, &image, &backup, true).unwrap_err();
            assert!(err.contains(expected), "case {index}: {err}");
            assert_eq!(target.data, before);
            assert!(!backup.exists());
        }
        // Existing backup file is never overwritten.
        let mut target = disk(2048, 0x0C);
        let before = target.data.clone();
        let backup = temp("exists.bak");
        fs::write(&backup, b"keep").unwrap();
        let err = install(&mut target, &image, &backup, true).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(&backup).unwrap(), b"keep");
        assert_eq!(target.data, before);
        fs::remove_file(backup).unwrap();
    }

    #[test]
    fn image_validation() {
        assert!(validate_image(&vec![0u8; 120 * 512]).is_err());
        let mut unsigned = image();
        unsigned[510] = 0;
        assert!(validate_image(&unsigned).is_err());
        assert!(validate_image(&image()[..121 * 512]).is_ok());
        // Any other signed boot sector is not a BootApp image.
        let mut foreign = image();
        foreign[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert!(
            validate_image(&foreign)
                .unwrap_err()
                .contains("stage1 signature")
        );
        let mut empty_stage2 = image();
        empty_stage2[512..121 * 512].fill(0);
        assert!(
            validate_image(&empty_stage2)
                .unwrap_err()
                .contains("stage2")
        );
    }

    #[test]
    fn failed_gap_write_leaves_lba0_untouched() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        target.fail_writes_at = Some(1);
        let original = target.data.clone();
        let backup = temp("fail.bak");
        let err = install(&mut target, &image, &backup, true).unwrap_err();
        assert!(err.contains("LBA0 was NOT modified"), "{err}");
        assert_eq!(target.data, original);
        let saved = fs::read(&backup).unwrap();
        assert_eq!(decode_backup(&saved).unwrap().0, &original[..REGION_BYTES]);
        fs::remove_file(backup).unwrap();
    }

    #[test]
    fn interrupted_install_is_restorable_and_reinstallable() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        target.fail_writes_at = Some(SAVED_MBR_LBA as u64);
        let original = target.data.clone();
        let backup = temp("partial.bak");
        assert!(install(&mut target, &image, &backup, true).is_err());
        assert_eq!(&target.data[..512], &original[..512], "LBA0 untouched");
        assert_eq!(
            &target.data[512..121 * 512],
            &image[512..121 * 512],
            "gap partially written"
        );
        // A re-install is refused (gap in use) but uninstall cleans up.
        let again = temp("partial-again.bak");
        assert!(
            install(&mut target, &image, &again, true)
                .unwrap_err()
                .contains("post-MBR gap")
        );
        target.fail_writes_at = None;
        let saved = fs::read(&backup).unwrap();
        uninstall(&mut target, &saved, false, true).unwrap();
        assert_eq!(target.data, original);
        install(&mut target, &image, &again, true).unwrap();
        fs::remove_file(backup).unwrap();
        fs::remove_file(again).unwrap();
    }

    #[test]
    fn later_boot_loader_is_not_destroyed() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        let original = target.data.clone();
        let backup = temp("grub.bak");
        install(&mut target, &image, &backup, true).unwrap();
        // Simulate a GRUB install afterwards: new boot code and core.img in LBA1..60.
        target.data[..440].fill(0x90);
        for lba in 1..=60 {
            target.data[lba * 512..(lba + 1) * 512].fill(0x47);
        }
        let saved = fs::read(&backup).unwrap();
        let grub = target.data.clone();
        let err = uninstall(&mut target, &saved, false, true).unwrap_err();
        assert!(
            err.contains("LBA0 boot code") && err.contains("LBA 1..=60"),
            "{err}"
        );
        assert_eq!(target.data, grub, "refused uninstall must not write");
        // Only foreign gap content (BootApp LBA0 still active) is refused too.
        target.data[..440].copy_from_slice(&image[..440]);
        let err = uninstall(&mut target, &saved, false, true).unwrap_err();
        assert!(
            !err.contains("LBA0 boot code") && err.contains("LBA 1..=60"),
            "{err}"
        );
        // --force restores the backup anyway.
        uninstall(&mut target, &saved, true, true).unwrap();
        assert_eq!(target.data, original);
        fs::remove_file(backup).unwrap();
    }

    #[test]
    fn mismatched_backup_is_refused() {
        let image = image();
        let mut target = disk(2048, 0x0C);
        let backup = temp("mismatch.bak");
        install(&mut target, &image, &backup, true).unwrap();
        let saved = fs::read(&backup).unwrap();
        let before = target.data.clone();
        let mut edited = saved.clone();
        edited[SECTOR + 5] ^= 0xFF; // original LBA0 byte changed, header not updated
        assert!(
            uninstall(&mut target, &edited, false, true)
                .unwrap_err()
                .contains("checksums")
        );
        let mut header = saved.clone();
        header[0] = b'X';
        assert!(uninstall(&mut target, &header, false, true).is_err());
        assert!(uninstall(&mut target, &saved[..REGION_BYTES], false, true).is_err());
        // A backup of a different disk does not match this target.
        let mut other_disk = disk(4096, 0x83);
        let other_backup = temp("other.bak");
        install(&mut other_disk, &image, &other_backup, true).unwrap();
        let other = fs::read(&other_backup).unwrap();
        assert!(uninstall(&mut target, &other, false, true).is_err());
        assert_eq!(target.data, before);
        fs::remove_file(other_backup).unwrap();
        fs::remove_file(backup).unwrap();
    }
}
