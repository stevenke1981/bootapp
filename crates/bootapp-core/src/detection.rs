//! Conservative loader discovery hints. Identifying a loader is not evidence
//! that its operating system is installed, intact, or successfully bootable.

use alloc::{vec, vec::Vec};

use crate::{validate_efi_path, Architecture};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsKind {
    Windows,
    Linux,
    MacOs,
    Unknown,
}

impl OsKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::Unknown => "Unknown",
        }
    }
}

/// Classify a syntactically valid loader path, using case-insensitive EFI names.
/// Unknown paths return `None`; standard removable-media loaders are explicitly
/// `Unknown`. macOS paths, including APFS Preboot UUID and recovery paths, still
/// require filesystem/firmware access; recognizing them does not add an APFS or
/// HFS+ driver or establish that a Mac can boot the candidate.
pub fn classify_loader_path(path: &str) -> Option<OsKind> {
    if !validate_efi_path(path) {
        return None;
    }
    let normalized = path.to_ascii_lowercase();
    let name = normalized.rsplit('\\').next()?;
    if name == "bootmgfw.efi" {
        Some(OsKind::Windows)
    } else if name.starts_with("grub")
        || name.starts_with("shim")
        || name.starts_with("systemd-boot")
        || normalized.starts_with("\\efi\\linux\\")
    {
        Some(OsKind::Linux)
    } else if normalized == "\\system\\library\\coreservices\\boot.efi"
        || (normalized.starts_with("\\efi\\apple\\") && name == "boot.efi")
        || normalized == "\\com.apple.recovery.boot\\boot.efi"
        || macos_preboot_path(&normalized)
    {
        Some(OsKind::MacOs)
    } else if normalized.starts_with("\\efi\\boot\\")
        && normalized["\\efi\\boot\\".len()..].find('\\').is_none()
        && name.starts_with("boot")
    {
        Some(OsKind::Unknown)
    } else {
        None
    }
}

fn macos_preboot_path(normalized: &str) -> bool {
    let Some((uuid, remainder)) = normalized
        .strip_prefix('\\')
        .and_then(|s| s.split_once('\\'))
    else {
        return false;
    };
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && remainder == "system\\library\\coreservices\\boot.efi"
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

/// Read a PE loader's architecture without dereferencing untrusted offsets.
/// A complete DOS header and declared optional header must be present, machine
/// and PE32/PE32+ must agree, and Subsystem must be EFI_APPLICATION (10).
/// This is a header probe, not an executable verifier: firmware LoadImage must
/// still validate sections, relocation, signatures, and executable permissions.
pub fn pe_architecture(bytes: &[u8]) -> Option<Architecture> {
    if bytes.get(..2)? != b"MZ" {
        return None;
    }
    let pe = usize::try_from(read_u32(bytes, 0x3c)?).ok()?;
    if pe < 64 || bytes.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe.checked_add(4)?;
    let machine = read_u16(bytes, coff)?;
    let optional_size = usize::from(read_u16(bytes, coff.checked_add(16)?)?);
    let optional_start = coff.checked_add(20)?;
    let optional = bytes.get(optional_start..optional_start.checked_add(optional_size)?)?;
    let magic = read_u16(optional, 0)?;
    let arch = match (machine, magic) {
        (0x014c, 0x010b) if optional_size >= 96 => Architecture::X86,
        (0x8664, 0x020b) if optional_size >= 112 => Architecture::X64,
        (0xaa64, 0x020b) if optional_size >= 112 => Architecture::Aarch64,
        _ => return None,
    };
    if read_u16(optional, 68)? != 10 {
        return None;
    }
    Some(arch)
}

/// Common paths on a firmware-readable filesystem, in deterministic order.
/// Vendors and UKI names are open-ended: callers must also enumerate EFI vendor
/// directories and EFI/Linux for recognized loader filenames.
pub fn loader_candidates(arch: Architecture) -> Vec<(&'static str, OsKind)> {
    let mut paths = vec![("\\EFI\\Microsoft\\Boot\\bootmgfw.efi", OsKind::Windows)];
    let linux: &[&str] = match arch {
        Architecture::X86 => &[
            "\\EFI\\ubuntu\\shimia32.efi",
            "\\EFI\\ubuntu\\grubia32.efi",
            "\\EFI\\debian\\shimia32.efi",
            "\\EFI\\debian\\grubia32.efi",
            "\\EFI\\fedora\\shimia32.efi",
            "\\EFI\\fedora\\grubia32.efi",
            "\\EFI\\opensuse\\shimia32.efi",
            "\\EFI\\opensuse\\grubia32.efi",
            "\\EFI\\systemd\\systemd-bootia32.efi",
            "\\EFI\\Linux\\linuxia32.efi",
        ],
        Architecture::X64 => &[
            "\\EFI\\ubuntu\\shimx64.efi",
            "\\EFI\\ubuntu\\grubx64.efi",
            "\\EFI\\debian\\shimx64.efi",
            "\\EFI\\debian\\grubx64.efi",
            "\\EFI\\fedora\\shimx64.efi",
            "\\EFI\\fedora\\grubx64.efi",
            "\\EFI\\opensuse\\shim.efi",
            "\\EFI\\opensuse\\grubx64.efi",
            "\\EFI\\systemd\\systemd-bootx64.efi",
            "\\EFI\\Linux\\linuxx64.efi",
        ],
        Architecture::Aarch64 => &[
            "\\EFI\\ubuntu\\shimaa64.efi",
            "\\EFI\\ubuntu\\grubaa64.efi",
            "\\EFI\\debian\\shimaa64.efi",
            "\\EFI\\debian\\grubaa64.efi",
            "\\EFI\\fedora\\shimaa64.efi",
            "\\EFI\\fedora\\grubaa64.efi",
            "\\EFI\\opensuse\\shimaa64.efi",
            "\\EFI\\opensuse\\grubaa64.efi",
            "\\EFI\\systemd\\systemd-bootaa64.efi",
            "\\EFI\\Linux\\linuxaa64.efi",
        ],
    };
    paths.extend(linux.iter().map(|&path| (path, OsKind::Linux)));
    // macOS boot.efi is an Intel-era loader. Apple Silicon uses a different
    // boot chain; these candidates must not imply AArch64 Mac support.
    if arch == Architecture::X64 {
        paths.push(("\\System\\Library\\CoreServices\\boot.efi", OsKind::MacOs));
        paths.push(("\\EFI\\APPLE\\boot.efi", OsKind::MacOs));
    }
    let fallback = match arch {
        Architecture::X86 => "\\EFI\\BOOT\\BOOTIA32.EFI",
        Architecture::X64 => "\\EFI\\BOOT\\BOOTX64.EFI",
        Architecture::Aarch64 => "\\EFI\\BOOT\\BOOTAA64.EFI",
    };
    paths.push((fallback, OsKind::Unknown));
    paths
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|part| part == needle)
}

/// Probe the first 512-byte BIOS sector, not arbitrary disk/file text.
/// A valid signature with no recognized loader returns Unknown. NTFS recognition
/// additionally requires its exact OEM field, a boot jump and a BOOTMGR/NTLDR
/// marker in boot code. GRUB recognition requires a boot jump and its terminated
/// banner in the pre-partition-table code area. All results remain heuristics.
pub fn classify_legacy_boot_sector(bytes: &[u8]) -> Option<OsKind> {
    let sector = bytes.get(..512)?;
    if sector[510..512] != [0x55, 0xaa] {
        return None;
    }
    let has_boot_jump = (sector[0] == 0xeb && sector[2] == 0x90) || sector[0] == 0xe9;
    if !has_boot_jump {
        return Some(OsKind::Unknown);
    }
    if &sector[3..11] == b"NTFS    "
        && (contains(&sector[84..510], b"BOOTMGR") || contains(&sector[84..510], b"NTLDR"))
    {
        return Some(OsKind::Windows);
    }
    if [b"GRUB ".as_slice(), b"GRUB\0", b"GRUB\r", b"GRUB\n"]
        .iter()
        .any(|marker| contains(&sector[3..446], marker))
    {
        return Some(OsKind::Linux);
    }
    Some(OsKind::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe(machine: u16, magic: u16) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&machine.to_le_bytes());
        bytes[148..150].copy_from_slice(&240u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&magic.to_le_bytes());
        bytes[220..222].copy_from_slice(&10u16.to_le_bytes());
        bytes
    }

    #[test]
    fn classifies_loader_hints_without_generic_boot_false_positives() {
        for (path, kind) in [
            ("\\EFI\\MICROSOFT\\BOOT\\BOOTMGFW.EFI", OsKind::Windows),
            ("\\EFI\\ubuntu\\grubx64.efi", OsKind::Linux),
            ("\\EFI\\arbitrary-vendor\\shimx64.efi", OsKind::Linux),
            ("\\EFI\\systemd\\systemd-bootaa64.efi", OsKind::Linux),
            ("\\EFI\\Linux\\my-custom-kernel.efi", OsKind::Linux),
            ("\\System\\Library\\CoreServices\\boot.efi", OsKind::MacOs),
            ("\\EFI\\Apple\\Boot\\boot.efi", OsKind::MacOs),
            ("\\EFI\\BOOT\\BOOTIA32.EFI", OsKind::Unknown),
        ] {
            assert_eq!(classify_loader_path(path), Some(kind), "{path}");
        }
        for path in [
            "\\arbitrary\\boot.efi",
            "\\EFI\\APPLEOTHER\\boot.efi",
            "\\EFI\\BOOT\\OTHER\\BOOTX64.EFI",
            "\\EFI\\LinuxOther\\kernel.efi",
            "\\EFI\\BOOT\\shell.efi",
            "EFI\\ubuntu\\grubx64.efi",
            "\\EFI\\..\\grubx64.efi",
            "\\EFI/ubuntu/grubx64.efi",
            "\\EFI\\ubuntu\\shimx64.exe",
            "\\EFI\\ubuntu\\shimx64.efi\0",
        ] {
            assert_eq!(classify_loader_path(path), None, "{path}");
        }
    }

    #[test]
    fn recognizes_exact_macos_preboot_and_recovery_paths_only() {
        for path in [
            "\\00112233-4455-6677-8899-AaBbCcDdEeFf\\System\\Library\\CoreServices\\boot.efi",
            "\\abcdefab-cdef-abcd-efab-cdefabcdefab\\SYSTEM\\LIBRARY\\CORESERVICES\\BOOT.EFI",
            "\\com.apple.recovery.boot\\boot.efi",
            "\\COM.APPLE.RECOVERY.BOOT\\BOOT.EFI",
        ] {
            assert_eq!(classify_loader_path(path), Some(OsKind::MacOs), "{path}");
        }
        for path in [
            "\\00112233-4455-6677-8899-AaBbCcDdEeFg\\System\\Library\\CoreServices\\boot.efi",
            "\\001122334455-6677-8899-AaBbCcDdEeFf\\System\\Library\\CoreServices\\boot.efi",
            "\\00112233-4455-6677-8899-AaBbCcDdEeF\\System\\Library\\CoreServices\\boot.efi",
            "\\00112233-4455-6677-8899-AaBbCcDdEeFf0\\System\\Library\\CoreServices\\boot.efi",
            "\\00112233-4455-6677-8899-AaBbCcDdEeFf\\other\\boot.efi",
            "\\00112233-4455-6677-8899-AaBbCcDdEeFf\\System\\Library\\CoreServices\\extra\\boot.efi",
            "\\arbitrary\\System\\Library\\CoreServices\\boot.efi",
            "\\other\\com.apple.recovery.boot\\boot.efi",
            "\\com.apple.recovery.boot.backup\\boot.efi",
            "\\com.apple.recovery.boot\\extra\\boot.efi",
            "\\com.apple.recovery.boot\\other.efi",
            "\\EFI\\00112233-4455-6677-8899-AaBbCcDdEeFf\\System\\Library\\CoreServices\\boot.efi",
        ] {
            assert_eq!(classify_loader_path(path), None, "{path}");
        }
    }

    #[test]
    fn recognizes_machine_and_optional_header_combinations() {
        for (machine, magic, arch) in [
            (0x014c, 0x010b, Architecture::X86),
            (0x8664, 0x020b, Architecture::X64),
            (0xaa64, 0x020b, Architecture::Aarch64),
        ] {
            assert_eq!(pe_architecture(&pe(machine, magic)), Some(arch));
        }
        for (machine, magic) in [
            (0x014c, 0x020b),
            (0x8664, 0x010b),
            (0xaa64, 0x010b),
            (0, 0x020b),
        ] {
            assert_eq!(pe_architecture(&pe(machine, magic)), None);
        }
    }

    #[test]
    fn rejects_every_truncated_header_prefix_and_unbounded_offsets() {
        let good = pe(0x8664, 0x020b);
        for end in 0..392 {
            assert_eq!(pe_architecture(&good[..end]), None, "prefix {end}");
        }
        assert_eq!(pe_architecture(&good[..392]), Some(Architecture::X64));
        for offset in [0, 1, 63, 500, u32::MAX] {
            let mut bad = good.clone();
            bad[0x3c..0x40].copy_from_slice(&offset.to_le_bytes());
            assert_eq!(pe_architecture(&bad), None);
        }
        for (offset, byte) in [(0, b'X'), (128, b'X'), (130, 1), (220, 3)] {
            let mut bad = good.clone();
            bad[offset] = byte;
            assert_eq!(pe_architecture(&bad), None);
        }
        for size in [0u16, 68, 70, 95, 111, u16::MAX] {
            let mut bad = good.clone();
            bad[148..150].copy_from_slice(&size.to_le_bytes());
            assert_eq!(pe_architecture(&bad), None);
        }
    }

    #[test]
    fn candidates_are_valid_distinct_and_match_their_classification() {
        for arch in [Architecture::X86, Architecture::X64, Architecture::Aarch64] {
            let candidates = loader_candidates(arch);
            for (index, (path, kind)) in candidates.iter().enumerate() {
                assert!(validate_efi_path(path));
                assert_eq!(classify_loader_path(path), Some(*kind));
                assert!(!candidates[..index]
                    .iter()
                    .any(|(previous, _)| previous.eq_ignore_ascii_case(path)));
            }
            assert_eq!(
                candidates.first().map(|(_, kind)| *kind),
                Some(OsKind::Windows)
            );
            assert_eq!(
                candidates.last().map(|(_, kind)| *kind),
                Some(OsKind::Unknown)
            );
            if arch != Architecture::X64 {
                assert!(!candidates.iter().any(|(_, kind)| *kind == OsKind::MacOs));
            }
        }
        assert!(loader_candidates(Architecture::X86)
            .iter()
            .any(|(path, _)| path.ends_with("BOOTIA32.EFI")));
        assert!(loader_candidates(Architecture::Aarch64)
            .iter()
            .any(|(path, _)| path.ends_with("grubaa64.efi")));
        assert_eq!(OsKind::MacOs.label(), "macOS");
    }

    fn signed_sector() -> [u8; 512] {
        let mut bytes = [0; 512];
        bytes[..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
        bytes[510..].copy_from_slice(&[0x55, 0xaa]);
        bytes
    }

    #[test]
    fn legacy_recognition_requires_structure_and_relevant_markers() {
        let mut sector = signed_sector();
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        sector[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        sector[200..207].copy_from_slice(b"BOOTMGR");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Windows));
        sector[200..207].fill(0);
        sector[200..205].copy_from_slice(b"NTLDR");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Windows));
        sector[3..11].fill(0);
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        sector[200..205].copy_from_slice(b"GRUB ");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Linux));
        sector[0] = 0;
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        sector[0] = 0xeb;
        sector[510] = 0;
        assert_eq!(classify_legacy_boot_sector(&sector), None);
    }

    #[test]
    fn legacy_rejects_truncation_partition_text_and_unterminated_words() {
        let mut sector = signed_sector();
        for end in 0..512 {
            assert_eq!(classify_legacy_boot_sector(&sector[..end]), None);
        }
        sector[450..455].copy_from_slice(b"GRUB ");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        sector[200..206].copy_from_slice(b"GRUBBY");
        assert_eq!(classify_legacy_boot_sector(&sector), Some(OsKind::Unknown));
        let mut extra = sector.to_vec();
        extra.extend_from_slice(b"GRUB BOOTMGR NTFS    ");
        assert_eq!(classify_legacy_boot_sector(&extra), Some(OsKind::Unknown));
    }
}
