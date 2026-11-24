use bootapp_core::{Architecture, Config, TIMEOUT_DISABLED, VolumeSelector, grub};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

mod bios_install;
mod disk;
mod install;
mod nvram;
mod scan;
mod sha256;
#[cfg(windows)]
mod win;

const HELP: &str = r"BootApp 0.3 — Rust boot manager tools
  bootapp validate <config>                  (bootapp.cfg, or GRUB subset if named grub.cfg)
  bootapp preview <config> <x86|x64|aarch64>
  bootapp grub-check <grub.cfg> [x86|x64|aarch64]
  bootapp stage <config> <x86|x64|aarch64> <manager.efi> <new-output-directory>
  bootapp scan <x86|x64|aarch64> <volume-directory>...
  bootapp install uefi --esp DIR --arch A --manager FILE [--config FILE] [--fallback] [--register] [--apply]
  bootapp uninstall uefi --esp DIR [--apply]
  bootapp install bios --target PATH --bios-image FILE --backup NEWFILE [--allow-system-disk] [--apply]
  bootapp uninstall bios --target PATH --backup FILE [--allow-system-disk] [--force] [--apply]
  bootapp bootorder list
  bootapp bootorder set XXXX[,XXXX...] [--apply]
  bootapp bootorder next XXXX [--apply]
Every command that writes is a DRY RUN unless --apply is given: it prints the exact plan
(files, sectors, firmware variables with hex payloads) and changes nothing.
install uefi writes EFI\BOOTAPP\BOOTAPP<ARCH>.EFI (+ config, + EFI\BOOT fallback with backup)
and EFI\BOOTAPP\install-manifest.txt, which uninstall uefi uses to undo exactly those changes.
--esp may be any directory (no admin needed); on Windows a bare S: means the volume root S:\.
install bios / uninstall bios on a disk-image FILE work on every platform without admin.
A \\.\PhysicalDriveN target is Windows-only and needs an elevated prompt even for a dry
run (the sectors must be read to plan); disks holding the Windows volume or the firmware
boot partition are refused unless --allow-system-disk. The backup must be a NEW file and
is written before any disk sector. uninstall bios refuses if any sector holds neither
BootApp's nor the original bytes (e.g. GRUB installed later); --force overrides that and
is DANGEROUS (it destroys whatever is there now).
--register, uninstall uefi of a registered install, and bootorder (even list) use Windows
firmware-variable APIs and need an elevated prompt.
Scan reads mounted directories only; loader identification does not prove an OS boots.
Stage creates a new removable-media directory.";

fn architecture(value: &str) -> Result<Architecture, String> {
    Architecture::parse(value).ok_or_else(|| format!("unknown architecture: {value}"))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read at most `limit` bytes; larger files are an error, never truncated.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if data.len() as u64 > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(data)
}

fn read_text(path: &Path) -> Result<String, String> {
    let data = read_bounded(path, bootapp_core::MAX_CONFIG_BYTES as u64).map_err(|e| {
        if e.contains("exceeds") {
            "configuration exceeds 64 KiB".into()
        } else {
            e
        }
    })?;
    String::from_utf8(data).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_config(path: &Path) -> Result<(Config, String), String> {
    let text = read_text(path)?;
    let config = Config::parse(&text).map_err(|e| e.to_string())?;
    Ok((config, text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigKind {
    BootApp,
    Grub,
}

pub(crate) struct LoadedConfig {
    pub kind: ConfigKind,
    pub text: String,
    pub config: Config,
    pub warnings: Vec<String>,
}

fn is_grub_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("grub.cfg"))
}

/// bootapp.cfg, or the GRUB subset when the file is named `grub.cfg`.
pub(crate) fn load_any_config(path: &Path) -> Result<LoadedConfig, String> {
    let text = read_text(path)?;
    if is_grub_name(path) {
        let parsed = grub::parse(&text).map_err(|e| e.to_string())?;
        let warnings = parsed
            .warnings
            .iter()
            .map(|w| format!("grub.cfg line {}: {}", w.line, w.message))
            .collect();
        Ok(LoadedConfig {
            kind: ConfigKind::Grub,
            text,
            config: parsed.config,
            warnings,
        })
    } else {
        let config = Config::parse(&text).map_err(|e| e.to_string())?;
        Ok(LoadedConfig {
            kind: ConfigKind::BootApp,
            text,
            config,
            warnings: Vec::new(),
        })
    }
}

fn timeout_text(timeout: u32) -> String {
    if timeout == TIMEOUT_DISABLED {
        "timeout disabled".into()
    } else {
        format!("timeout {timeout} seconds")
    }
}

fn volume_text(volume: &VolumeSelector) -> String {
    match volume {
        VolumeSelector::Own => "own".into(),
        VolumeSelector::File(path) => format!("file:{path}"),
        VolumeSelector::FsUuid(uuid) => format!("fs-uuid:{uuid}"),
        VolumeSelector::Label(label) => format!("label:{label}"),
    }
}

fn grub_check(path: &Path, arch: Option<Architecture>) -> Result<(), String> {
    let text = read_text(path)?;
    let parsed = grub::parse(&text).map_err(|e| e.to_string())?;
    let config = &parsed.config;
    for warning in &parsed.warnings {
        println!(
            "warning: grub.cfg line {}: {}",
            warning.line, warning.message
        );
    }
    let entries: Vec<_> = match arch {
        Some(arch) => config.entries_for(arch),
        None => config.entries.iter().collect(),
    };
    println!(
        "GRUB subset OK: {} entries{}, default {}, {}, {} warning(s)",
        entries.len(),
        arch.map(|a| format!(" for {}", a.as_str()))
            .unwrap_or_default(),
        config.default,
        timeout_text(config.timeout),
        parsed.warnings.len()
    );
    for entry in entries {
        println!(
            "{} [{}] {} | volume={} | path={} | options=\"{}\" | arch={}",
            if entry.id == config.default { ">" } else { " " },
            entry.id,
            entry.title,
            volume_text(&entry.volume),
            entry.path,
            entry.options,
            entry.arch.map_or("any", |a| a.as_str())
        );
    }
    Ok(())
}

/// `--key value` / `--flag` parser for install, uninstall and bootorder commands.
struct Flags {
    values: Vec<(&'static str, String)>,
    switches: Vec<&'static str>,
}

impl Flags {
    fn parse(
        args: &[String],
        value_keys: &[&'static str],
        switch_keys: &[&'static str],
    ) -> Result<Self, String> {
        let mut flags = Flags {
            values: Vec::new(),
            switches: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if let Some(&key) = value_keys.iter().find(|k| **k == arg) {
                let value = iter
                    .next()
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("{key} requires a value"))?;
                if flags.values.iter().any(|(k, _)| *k == key) {
                    return Err(format!("{key} given twice"));
                }
                flags.values.push((key, value.clone()));
            } else if let Some(&key) = switch_keys.iter().find(|k| **k == arg) {
                if flags.switches.contains(&key) {
                    return Err(format!("{key} given twice"));
                }
                flags.switches.push(key);
            } else {
                return Err(format!("unexpected argument {arg:?}\n\n{HELP}"));
            }
        }
        Ok(flags)
    }
    fn value(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
    fn required(&self, key: &str) -> Result<&str, String> {
        self.value(key).ok_or_else(|| format!("{key} is required"))
    }
    fn has(&self, key: &str) -> bool {
        self.switches.contains(&key)
    }
}

fn apply_note(apply: bool) {
    if !apply {
        println!("(dry run: no changes will be made; add --apply to perform)");
    }
}

fn install_command(target: &str, rest: &[String]) -> Result<(), String> {
    match target {
        "uefi" => {
            let f = Flags::parse(
                rest,
                &["--esp", "--arch", "--manager", "--config"],
                &["--fallback", "--register", "--apply"],
            )?;
            apply_note(f.has("--apply"));
            install::install(&install::UefiInstall {
                esp: PathBuf::from(f.required("--esp")?),
                arch: architecture(f.required("--arch")?)?,
                manager: PathBuf::from(f.required("--manager")?),
                config: f.value("--config").map(PathBuf::from),
                fallback: f.has("--fallback"),
                register: f.has("--register"),
                apply: f.has("--apply"),
            })
        }
        "bios" => {
            let f = Flags::parse(
                rest,
                &["--target", "--bios-image", "--backup"],
                &["--allow-system-disk", "--apply"],
            )?;
            let apply = f.has("--apply");
            apply_note(apply);
            let image = bios_install::read_image_file(Path::new(f.required("--bios-image")?))?;
            let target = f.required("--target")?;
            let backup = Path::new(f.required("--backup")?);
            let mut disk = disk::open_target(target, apply, f.has("--allow-system-disk"))?;
            println!("Target {target}: {}", disk.description);
            bios_install::install(&mut disk, &image, backup, apply)
        }
        other => Err(format!(
            "unknown install target {other:?} (expected uefi or bios)"
        )),
    }
}

fn uninstall_command(target: &str, rest: &[String]) -> Result<(), String> {
    match target {
        "uefi" => {
            let f = Flags::parse(rest, &["--esp"], &["--apply"])?;
            apply_note(f.has("--apply"));
            install::uninstall(Path::new(f.required("--esp")?), f.has("--apply"))
        }
        "bios" => {
            let f = Flags::parse(
                rest,
                &["--target", "--backup"],
                &["--allow-system-disk", "--force", "--apply"],
            )?;
            let apply = f.has("--apply");
            apply_note(apply);
            let backup = read_bounded(
                Path::new(f.required("--backup")?),
                bios_install::BACKUP_FILE_BYTES as u64,
            )?;
            let target = f.required("--target")?;
            let mut disk = disk::open_target(target, apply, f.has("--allow-system-disk"))?;
            println!("Target {target}: {}", disk.description);
            bios_install::uninstall(&mut disk, &backup, f.has("--force"), apply)
        }
        other => Err(format!(
            "unknown uninstall target {other:?} (expected uefi or bios)"
        )),
    }
}

fn bootorder_command(rest: &[String]) -> Result<(), String> {
    let (plan, apply) = match rest {
        [list] if list == "list" => {
            let vars = nvram::system_vars()?;
            print!("{}", nvram::render_list(vars.as_ref())?);
            return Ok(());
        }
        [cmd, value, flags @ ..] if cmd == "set" || cmd == "next" => {
            let f = Flags::parse(flags, &[], &["--apply"])?;
            let vars = nvram::system_vars()?;
            let plan = if cmd == "set" {
                nvram::plan_set_order(vars.as_ref(), value)?
            } else {
                nvram::plan_boot_next(vars.as_ref(), value)?
            };
            (plan, f.has("--apply"))
        }
        _ => return Err(HELP.into()),
    };
    println!("Plan (firmware variables):");
    nvram::print_changes(&plan);
    if !apply {
        println!("DRY RUN: nothing was written. Re-run with --apply to perform this plan.");
        return Ok(());
    }
    let mut vars = nvram::system_vars()?;
    nvram::apply_changes(vars.as_mut(), &plan)
}

pub(crate) fn efi_name(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86 => "BOOTIA32.EFI",
        Architecture::X64 => "BOOTX64.EFI",
        Architecture::Aarch64 => "BOOTAA64.EFI",
    }
}

pub(crate) fn validate_pe(bytes: &[u8], arch: Architecture) -> Result<(), String> {
    if bootapp_core::detection::pe_architecture(bytes) == Some(arch) {
        Ok(())
    } else {
        Err("not a matching EFI application PE image".to_string())
    }
}

fn stage(
    config_path: &Path,
    arch: Architecture,
    binary: &Path,
    output: &Path,
) -> Result<(), String> {
    let (config, text) = read_config(config_path)?;
    if config.entries_for(arch).is_empty() {
        return Err("no entries for requested architecture".into());
    }
    let image = fs::read(binary).map_err(|e| e.to_string())?;
    validate_pe(&image, arch)?;
    // create_dir is deliberately exclusive: never overwrite an existing ESP or staging tree.
    fs::create_dir(output).map_err(|e| {
        format!(
            "cannot create NEW output directory {}: {e}",
            output.display()
        )
    })?;
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(output.join("EFI/BOOT"))?;
        fs::create_dir_all(output.join("EFI/BOOTAPP"))?;
        fs::write(output.join("EFI/BOOT").join(efi_name(arch)), &image)?;
        fs::write(output.join("EFI/BOOTAPP/bootapp.cfg"), text)?;
        Ok(())
    };
    write().map_err(|e| format!("staging incomplete at {}: {e}", output.display()))?;
    println!(
        "Staged {} in {}. Copy your configured target EFI applications onto this volume before booting.",
        arch.as_str(),
        output.display()
    );
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [] => {
            println!("{HELP}");
            Ok(())
        }
        [help] if help == "--help" || help == "-h" => {
            println!("{HELP}");
            Ok(())
        }
        [command, path] if command == "validate" => {
            let loaded = load_any_config(Path::new(path))?;
            for warning in &loaded.warnings {
                println!("warning: {warning}");
            }
            println!(
                "Valid{}: {} entries, {}",
                if loaded.kind == ConfigKind::Grub {
                    " GRUB subset"
                } else {
                    ""
                },
                loaded.config.entries.len(),
                timeout_text(loaded.config.timeout)
            );
            Ok(())
        }
        [command, path] if command == "grub-check" => grub_check(Path::new(path), None),
        [command, path, arch] if command == "grub-check" => {
            grub_check(Path::new(path), Some(architecture(arch)?))
        }
        [command, target, rest @ ..] if command == "install" => install_command(target, rest),
        [command, target, rest @ ..] if command == "uninstall" => uninstall_command(target, rest),
        [command, rest @ ..] if command == "bootorder" => bootorder_command(rest),
        [command, path, arch] if command == "preview" => {
            let arch = architecture(arch)?;
            let config = load_any_config(Path::new(path))?.config;
            let entries = config.entries_for(arch);
            let default = config
                .default_index(arch)
                .ok_or("no entries for this architecture")?;
            println!(
                "BootApp | {} | {}",
                arch.as_str(),
                timeout_text(config.timeout)
            );
            for (index, entry) in entries.iter().enumerate() {
                println!(
                    "{} {} — {}",
                    if index == default { ">" } else { " " },
                    entry.title,
                    entry.path
                );
            }
            Ok(())
        }
        [command, config, arch, binary, output] if command == "stage" => stage(
            Path::new(config),
            architecture(arch)?,
            Path::new(binary),
            Path::new(output),
        ),
        [command, arch, volumes @ ..] if command == "scan" && !volumes.is_empty() => {
            let arch = architecture(arch)?;
            let report = scan::scan_volumes(arch, volumes.iter().map(Path::new))?;
            for entry in &report.entries {
                println!(
                    "{} | volume={} | {} | {}",
                    entry.kind.label(),
                    entry.volume.display(),
                    entry.path,
                    arch.as_str()
                );
            }
            if report.entries.is_empty() {
                println!(
                    "No matching {} EFI loaders found in the supplied directories.",
                    arch.as_str()
                );
            } else {
                println!(
                    "{} loader candidate(s); OS bootability has not been tested.",
                    report.entries.len()
                );
            }
            if report.skipped_links != 0 {
                println!("Skipped {} symlink/reparse point(s).", report.skipped_links);
            }
            for warning in report.warnings {
                eprintln!("Scan warning: {warning}");
            }
            Ok(())
        }
        _ => Err(HELP.into()),
    }
}

fn main() -> ExitCode {
    match run(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("BootApp: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn pe(arch: Architecture) -> Vec<u8> {
        let mut bytes = vec![0u8; 256];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        let machine = match arch {
            Architecture::X86 => 0x014cu16,
            Architecture::X64 => 0x8664,
            Architecture::Aarch64 => 0xaa64,
        };
        bytes[68..70].copy_from_slice(&machine.to_le_bytes());
        bytes[84..86].copy_from_slice(&112u16.to_le_bytes());
        let magic = if arch == Architecture::X86 {
            0x010bu16
        } else {
            0x020b
        };
        bytes[88..90].copy_from_slice(&magic.to_le_bytes());
        bytes[156..158].copy_from_slice(&10u16.to_le_bytes());
        bytes
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flag_values_cannot_be_flags() {
        let keys = ["--target", "--backup"];
        let switches = ["--apply"];
        let err = Flags::parse(&strings(&["--backup", "--apply"]), &keys, &switches)
            .err()
            .unwrap();
        assert!(err.contains("--backup requires a value"), "{err}");
        assert!(Flags::parse(&strings(&["--backup"]), &keys, &switches).is_err());
        assert!(Flags::parse(&strings(&["--apply", "--apply"]), &keys, &switches).is_err());
        assert!(Flags::parse(&strings(&["--bogus"]), &keys, &switches).is_err());
        let f = Flags::parse(
            &strings(&["--backup", "-b.bin", "--apply"]),
            &keys,
            &switches,
        )
        .unwrap();
        assert_eq!(f.value("--backup"), Some("-b.bin"));
        assert!(f.has("--apply"));
        // Through the command dispatcher: nothing is created, the error is explicit.
        let err = run(&strings(&[
            "install",
            "bios",
            "--target",
            "x.img",
            "--bios-image",
            "y.img",
            "--backup",
            "--apply",
        ]))
        .unwrap_err();
        assert!(err.contains("--backup requires a value"), "{err}");
    }

    #[test]
    fn rejects_wrong_architecture_and_non_efi_subsystem() {
        for arch in [Architecture::X86, Architecture::X64, Architecture::Aarch64] {
            assert!(validate_pe(&pe(arch), arch).is_ok());
        }
        assert!(validate_pe(&pe(Architecture::X86), Architecture::X64).is_err());
        let mut image = pe(Architecture::X64);
        image[156] = 3;
        assert!(validate_pe(&image, Architecture::X64).is_err());
    }

    #[test]
    fn rejects_truncated_or_overflowing_pe_headers() {
        let image = pe(Architecture::X64);
        for end in 0..200 {
            assert!(validate_pe(&image[..end], Architecture::X64).is_err());
        }
        let mut malformed = image;
        malformed[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_pe(&malformed, Architecture::X64).is_err());
    }

    #[test]
    fn stage_preserves_existing_destination() {
        let path = env::temp_dir().join(format!("bootapp-stage-test-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let cfg = path.join("input.cfg");
        fs::write(&cfg, "[entry test]\ntitle=Test\npath=\\EFI\\test.efi\n").unwrap();
        let binary = path.join("test.efi");
        fs::write(&binary, pe(Architecture::X64)).unwrap();
        let new_output = path.join("new");
        assert!(stage(&cfg, Architecture::X64, &binary, &new_output).is_ok());
        let image_path = new_output.join("EFI/BOOT/BOOTX64.EFI");
        fs::write(&image_path, b"preserve me").unwrap();
        assert!(stage(&cfg, Architecture::X64, &binary, &new_output).is_err());
        assert_eq!(fs::read(image_path).unwrap(), b"preserve me");
        fs::remove_dir_all(path).unwrap();
    }
}
