//! Pure volume-identity rules for GRUB `search --fs-uuid` / `--label`.
//!
//! Host-testable: no firmware access. The firmware side reads LBA 0 of each
//! candidate partition and the FAT driver's volume label, then asks these
//! functions whether they match the configured selector.

use alloc::{format, string::String};

/// GRUB-compatible FAT volume serial (`search --fs-uuid`), formatted as
/// `XXXX-XXXX` (high 16 bits, low 16 bits, uppercase hex).
///
/// `sector` is LBA 0 of the partition. Returns `None` unless the boot sector
/// looks like a FAT BPB, so NTFS/exFAT/ext and blank media never match.
/// FAT32 is recognized by BPB_FATSz16 == 0 (serial at 0x43); FAT12/16
/// otherwise (serial at 0x27).
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub fn fat_volume_serial(sector: &[u8]) -> Option<String> {
    let sector = sector.get(..512)?;
    let u16_at = |offset: usize| u16::from_le_bytes([sector[offset], sector[offset + 1]]);
    let bytes_per_sector = u16_at(0x0B);
    let sectors_per_cluster = sector[0x0D];
    if !matches!(sector[0], 0xEB | 0xE9)
        || sector[510..512] != [0x55, 0xAA]
        || !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        || !sectors_per_cluster.is_power_of_two()
        || u16_at(0x0E) == 0
        || sector[0x10] == 0
    {
        return None;
    }
    let offset = if u16_at(0x16) == 0 { 0x43 } else { 0x27 };
    let serial = u32::from_le_bytes([
        sector[offset],
        sector[offset + 1],
        sector[offset + 2],
        sector[offset + 3],
    ]);
    Some(format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF))
}

/// FAT serial comparison is case-insensitive (`abcd-1234` == `ABCD-1234`).
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub fn fs_uuid_matches(found: &str, wanted: &str) -> bool {
    found.eq_ignore_ascii_case(wanted)
}

/// Volume labels compare case-insensitively (Unicode lowercase), ignoring the
/// trailing space/NUL padding FAT stores in its 11-byte label fields.
/// An empty label never matches.
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub fn label_matches(found: &str, wanted: &str) -> bool {
    let found = found.trim_end_matches([' ', '\0']);
    let wanted = wanted.trim_end_matches([' ', '\0']);
    !found.is_empty() && found.to_lowercase() == wanted.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bpb(fat_sz16: u16) -> [u8; 512] {
        let mut sector = [0u8; 512];
        sector[0] = 0xEB;
        sector[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        sector[0x0D] = 4;
        sector[0x0E..0x10].copy_from_slice(&1u16.to_le_bytes());
        sector[0x10] = 2;
        sector[0x16..0x18].copy_from_slice(&fat_sz16.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        sector
    }

    #[test]
    fn fat16_serial_at_0x27() {
        let mut sector = bpb(32);
        sector[0x27..0x2B].copy_from_slice(&0x1234_ABCDu32.to_le_bytes());
        // A decoy at the FAT32 offset must be ignored for FAT12/16.
        sector[0x43..0x47].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(fat_volume_serial(&sector).as_deref(), Some("1234-ABCD"));
    }

    #[test]
    fn fat32_serial_at_0x43() {
        let mut sector = bpb(0);
        sector[0x43..0x47].copy_from_slice(&0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(fat_volume_serial(&sector).as_deref(), Some("0A0B-0C0D"));
    }

    #[test]
    fn non_fat_sectors_are_rejected() {
        assert_eq!(fat_volume_serial(&[0u8; 512]), None);
        assert_eq!(fat_volume_serial(&bpb(1)[..511]), None);
        let mut unsigned = bpb(1);
        unsigned[511] = 0;
        assert_eq!(fat_volume_serial(&unsigned), None);
        let mut ntfs = bpb(0);
        ntfs[0x0E] = 0;
        ntfs[0x10] = 0;
        assert_eq!(fat_volume_serial(&ntfs), None);
        let mut exfat = bpb(0);
        exfat[0x0B] = 0;
        exfat[0x0C] = 0;
        assert_eq!(fat_volume_serial(&exfat), None);
        let mut odd_cluster = bpb(1);
        odd_cluster[0x0D] = 3;
        assert_eq!(fat_volume_serial(&odd_cluster), None);
    }

    #[test]
    fn uuid_and_label_comparisons_are_case_insensitive() {
        assert!(fs_uuid_matches("ABCD-12EF", "abcd-12ef"));
        assert!(!fs_uuid_matches("ABCD-12EF", "ABCD-12E0"));
        assert!(label_matches("BOOTAPP2   ", "bootapp2"));
        assert!(label_matches("Données", "DONNÉES"));
        assert!(!label_matches("", ""));
        assert!(!label_matches("ONE", "TWO"));
    }
}
