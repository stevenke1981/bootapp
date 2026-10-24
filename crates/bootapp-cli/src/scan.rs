//! Bounded, read-only discovery on stable mounted directory trees.
//! Symlinks and Windows reparse points are skipped, including root ancestors.
//! Like ordinary path-based filesystem tools, this does not provide isolation
//! against another process replacing directories during a scan.

use bootapp_core::{
    Architecture,
    detection::{OsKind, classify_loader_path, pe_architecture},
};
use std::{
    collections::BTreeSet,
    fs::{self, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_DEPTH: usize = 8;
const MAX_VISITED: usize = 4096;
const MAX_PROBES: usize = 1024;
const MAX_PE_BYTES: u64 = 65536;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub volume: PathBuf,
    pub path: String,
    pub kind: OsKind,
}

#[derive(Debug, Default)]
pub(crate) struct ScanReport {
    pub entries: Vec<Candidate>,
    pub skipped_links: usize,
    pub warnings: Vec<String>,
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    false
}

fn canonical_volume(path: &Path) -> Result<PathBuf, String> {
    // Inspect ancestors before canonicalizing, because canonicalize would hide
    // any link used to reach the requested volume directory.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err("scan volume cannot contain '..' components".into());
        }
        current.push(component.as_os_str());
        // A Windows drive prefix alone (C:) refers to a drive-relative current
        // directory; defer it until the following root component is appended.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|e| format!("{}: {e}", current.display()))?;
        if is_link(&metadata) {
            return Err(format!(
                "scan volume uses a symlink/reparse point: {}",
                current.display()
            ));
        }
    }
    let root = fs::canonicalize(&absolute).map_err(|e| format!("{}: {e}", absolute.display()))?;
    if !fs::metadata(&root).map_err(|e| e.to_string())?.is_dir() {
        return Err(format!(
            "scan volume is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

struct VolumeScan<'a> {
    root: &'a Path,
    arch: Architecture,
    report: &'a mut ScanReport,
    visited: usize,
    probes: usize,
    found: BTreeSet<String>,
}

impl VolumeScan<'_> {
    fn directory(&mut self, directory: &Path, depth: usize, system: bool) -> Result<(), String> {
        if depth > MAX_DEPTH {
            self.report
                .warnings
                .push(format!("depth limit reached at {}", directory.display()));
            return Ok(());
        }
        let mut children = Vec::new();
        let entries =
            fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        for entry in entries {
            if self.visited >= MAX_VISITED {
                return Err(format!(
                    "directory-entry limit ({MAX_VISITED}) reached on {}",
                    self.root.display()
                ));
            }
            self.visited += 1;
            children.push(entry.map_err(|e| e.to_string())?.path());
        }
        children.sort();
        for path in children {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.report
                        .warnings
                        .push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if is_link(&metadata) {
                self.report.skipped_links += 1;
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if depth == 0 {
                if metadata.is_dir() {
                    if name.eq_ignore_ascii_case("System") {
                        self.directory(&path, 1, true)?;
                    } else if !system && name.eq_ignore_ascii_case("EFI") {
                        self.directory(&path, 1, false)?;
                    } else if directory == self.root
                        && classify_loader_path(&format!(
                            "\\{name}\\System\\Library\\CoreServices\\boot.efi"
                        )) == Some(OsKind::MacOs)
                    {
                        // A single canonical UUID root. Reset the logical System
                        // depth so the existing exact Library/CoreServices walk
                        // is reused. Root identity prevents nested UUID resets.
                        self.directory(&path, 0, true)?;
                    } else if directory == self.root
                        && classify_loader_path(&format!("\\{name}\\boot.efi"))
                            == Some(OsKind::MacOs)
                    {
                        // Recovery is a leaf directory: only boot.efi is probed.
                        self.directory(&path, 3, true)?;
                    }
                }
            } else if metadata.is_dir() {
                if !system
                    || (depth == 1 && name.eq_ignore_ascii_case("Library"))
                    || (depth == 2 && name.eq_ignore_ascii_case("CoreServices"))
                {
                    self.directory(&path, depth + 1, system)?;
                }
            } else if metadata.is_file()
                && (!system || (depth == 3 && name.eq_ignore_ascii_case("boot.efi")))
            {
                self.probe(&path)?;
            }
        }
        Ok(())
    }

    fn probe(&mut self, file: &Path) -> Result<(), String> {
        let relative = file.strip_prefix(self.root).map_err(|e| e.to_string())?;
        let components: Option<Vec<_>> = relative
            .components()
            .map(|component| {
                if let Component::Normal(value) = component {
                    value.to_str()
                } else {
                    None
                }
            })
            .collect();
        let Some(components) = components else {
            return Ok(());
        };
        let path = format!("\\{}", components.join("\\"));
        let Some(kind) = classify_loader_path(&path) else {
            return Ok(());
        };
        if self.probes >= MAX_PROBES {
            return Err(format!(
                "PE probe limit ({MAX_PROBES}) reached on {}",
                self.root.display()
            ));
        }
        self.probes += 1;
        let probe = || -> std::io::Result<Vec<u8>> {
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // Open a reparse point itself if one appeared after the earlier
                // metadata check, rather than following its target.
                options.custom_flags(0x0020_0000);
            }
            let image = options.open(file)?;
            if is_link(&image.metadata()?) {
                return Err(std::io::Error::other(
                    "candidate became a symlink/reparse point",
                ));
            }
            let mut bytes = Vec::new();
            image.take(MAX_PE_BYTES).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        match probe() {
            Ok(bytes) if pe_architecture(&bytes) == Some(self.arch) => {
                if self.found.insert(path.to_ascii_lowercase()) {
                    self.report.entries.push(Candidate {
                        volume: self.root.to_path_buf(),
                        path,
                        kind,
                    });
                }
            }
            Ok(_) => {}
            Err(error) => self
                .report
                .warnings
                .push(format!("{}: {error}", file.display())),
        }
        Ok(())
    }
}

pub(crate) fn scan_volumes<'a>(
    arch: Architecture,
    volumes: impl IntoIterator<Item = &'a Path>,
) -> Result<ScanReport, String> {
    let mut report = ScanReport::default();
    let mut seen = BTreeSet::new();
    for volume in volumes {
        let root = canonical_volume(volume)?;
        #[cfg(windows)]
        let identity = root.as_os_str().to_ascii_lowercase();
        #[cfg(not(windows))]
        let identity = root.as_os_str().to_os_string();
        if !seen.insert(identity) {
            continue;
        }
        let mut scan = VolumeScan {
            root: &root,
            arch,
            report: &mut report,
            visited: 0,
            probes: 0,
            found: BTreeSet::new(),
        };
        if let Err(error) = scan.directory(&root, 0, false) {
            report.warnings.push(error);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct TestTree(PathBuf);
    impl TestTree {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "bootapp-scan-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn loader(&self, path: &str, arch: Architecture) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, crate::tests::pe(arch)).unwrap();
        }
    }
    impl Drop for TestTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn scan_discovers_case_insensitive_os_hints_and_filters_pe() {
        let tree = TestTree::new();
        tree.loader("eFi/Microsoft/Boot/BOOTMGFW.EFI", Architecture::X64);
        tree.loader("eFi/Ubuntu/grubx64.efi", Architecture::X64);
        tree.loader("eFi/Linux/custom-kernel.efi", Architecture::X64);
        tree.loader("System/Library/CoreServices/boot.efi", Architecture::X64);
        tree.loader("eFi/BOOT/BOOTX64.EFI", Architecture::X64);
        tree.loader("eFi/Debian/grubia32.efi", Architecture::X86);
        tree.loader("outside/grubx64.efi", Architecture::X64);
        tree.loader("eFi/Unknown/boot.efi", Architecture::X64);
        let invalid = tree.0.join("eFi/Ubuntu/shimx64.efi");
        let mut image = crate::tests::pe(Architecture::X64);
        image[156] = 3;
        fs::write(invalid, image).unwrap();
        let before = fs::metadata(tree.0.join("eFi/Ubuntu/grubx64.efi"))
            .unwrap()
            .modified()
            .unwrap();
        let report = scan_volumes(Architecture::X64, [tree.0.as_path()]).unwrap();
        assert!(report.warnings.is_empty());
        assert_eq!(report.entries.len(), 5);
        for kind in [
            OsKind::Windows,
            OsKind::Linux,
            OsKind::MacOs,
            OsKind::Unknown,
        ] {
            assert!(report.entries.iter().any(|entry| entry.kind == kind));
        }
        assert_eq!(
            fs::metadata(tree.0.join("eFi/Ubuntu/grubx64.efi"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[test]
    fn scan_detects_preboot_and_recovery_only_at_volume_root() {
        let tree = TestTree::new();
        let uuid = "01234567-89AB-cdef-0123-456789abcdef";
        tree.loader(
            &format!("{uuid}/System/Library/CoreServices/boot.efi"),
            Architecture::X64,
        );
        tree.loader("COM.APPLE.RECOVERY.BOOT/BOOT.EFI", Architecture::X64);
        tree.loader(
            "01234567-invalid/System/Library/CoreServices/boot.efi",
            Architecture::X64,
        );
        tree.loader(
            &format!("outside/{uuid}/System/Library/CoreServices/boot.efi"),
            Architecture::X64,
        );
        tree.loader(
            &format!("{uuid}/{uuid}/System/Library/CoreServices/boot.efi"),
            Architecture::X64,
        );
        let report = scan_volumes(Architecture::X64, [tree.0.as_path()]).unwrap();
        assert_eq!(report.entries.len(), 2);
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.kind == OsKind::MacOs)
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn identical_paths_on_distinct_volumes_remain_distinct() {
        let first = TestTree::new();
        let second = TestTree::new();
        first.loader("EFI/ubuntu/grubx64.efi", Architecture::X64);
        second.loader("EFI/ubuntu/grubx64.efi", Architecture::X64);
        let report = scan_volumes(
            Architecture::X64,
            [first.0.as_path(), second.0.as_path(), first.0.as_path()],
        )
        .unwrap();
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.entries[0].path, report.entries[1].path);
        assert_ne!(report.entries[0].volume, report.entries[1].volume);
    }

    #[test]
    fn empty_scan_succeeds_and_invalid_roots_fail() {
        let tree = TestTree::new();
        assert!(
            scan_volumes(Architecture::X64, [tree.0.as_path()])
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(scan_volumes(Architecture::X64, [tree.0.join("missing").as_path()]).is_err());
        fs::write(tree.0.join("file"), b"file").unwrap();
        assert!(scan_volumes(Architecture::X64, [tree.0.join("file").as_path()]).is_err());
    }

    #[test]
    fn recursion_is_bounded_and_reported() {
        let tree = TestTree::new();
        let deep = format!("EFI/{}/grubx64.efi", ["nested"; MAX_DEPTH + 1].join("/"));
        tree.loader(&deep, Architecture::X64);
        let report = scan_volumes(Architecture::X64, [tree.0.as_path()]).unwrap();
        assert!(report.entries.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("depth limit"))
        );
    }

    #[test]
    fn skips_directory_links_and_rejects_link_roots() {
        let tree = TestTree::new();
        let outside = TestTree::new();
        outside.loader("ubuntu/grubx64.efi", Architecture::X64);
        let link = tree.0.join("EFI");
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "junction creation failed: {:?}",
                output
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside.0, &link).unwrap();
        let report = scan_volumes(Architecture::X64, [tree.0.as_path()]).unwrap();
        assert!(report.entries.is_empty());
        assert_eq!(report.skipped_links, 1);
        assert!(scan_volumes(Architecture::X64, [link.as_path()]).is_err());
        assert!(scan_volumes(Architecture::X64, [link.join("ubuntu").as_path()]).is_err());
        #[cfg(windows)]
        fs::remove_dir(link).unwrap();
        #[cfg(unix)]
        fs::remove_file(link).unwrap();
        assert!(outside.0.join("ubuntu/grubx64.efi").exists());
    }
}
