use bootapp_core::{Architecture, Config};
use std::{env, fs, io::Read, path::Path, process::ExitCode};

mod scan;

const HELP: &str = "BootApp 0.2 — Rust boot manager tools
  bootapp validate <config>
  bootapp preview <config> <x86|x64|aarch64>
  bootapp stage <config> <x86|x64|aarch64> <manager.efi> <new-output-directory>
  bootapp scan <x86|x64|aarch64> <volume-directory>...
Scan reads mounted directories only; loader identification does not prove an OS boots.
Stage creates a new removable-media directory. It never installs to disks or changes firmware.";

fn architecture(value: &str) -> Result<Architecture, String> {
    Architecture::parse(value).ok_or_else(|| format!("unknown architecture: {value}"))
}

fn read_config(path: &Path) -> Result<(Config, String), String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::new();
    file.take(65537)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > 65536 {
        return Err("configuration exceeds 64 KiB".into());
    }
    let text = String::from_utf8(data).map_err(|e| e.to_string())?;
    let config = Config::parse(&text).map_err(|e| e.to_string())?;
    Ok((config, text))
}

fn efi_name(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86 => "BOOTIA32.EFI",
        Architecture::X64 => "BOOTX64.EFI",
        Architecture::Aarch64 => "BOOTAA64.EFI",
    }
}

fn validate_pe(bytes: &[u8], arch: Architecture) -> Result<(), String> {
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
            let (config, _) = read_config(Path::new(path))?;
            println!(
                "Valid: {} entries, timeout {} seconds",
                config.entries.len(),
                config.timeout
            );
            Ok(())
        }
        [command, path, arch] if command == "preview" => {
            let arch = architecture(arch)?;
            let (config, _) = read_config(Path::new(path))?;
            let entries = config.entries_for(arch);
            let default = config
                .default_index(arch)
                .ok_or("no entries for this architecture")?;
            println!("BootApp | {} | timeout {}s", arch.as_str(), config.timeout);
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
