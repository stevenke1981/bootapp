/// Self-path checks must include volume identity; another ESP can legitimately
/// contain a different executable at exactly the same removable-media pathname.
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub fn skip_self(path: &str, own_path: Option<&str>, same_volume: bool) -> bool {
    if !same_volume {
        return false;
    }
    if let Some(own) = own_path {
        return path.eq_ignore_ascii_case(own);
    }
    // A firmware may omit LoadedImage.FilePath. Keep other volumes usable while
    // conservatively suppressing fallback loaders on the manager's own volume.
    path.get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("\\EFI\\BOOT\\"))
}

#[cfg(test)]
mod tests {
    use super::skip_self;

    #[test]
    fn identical_path_on_other_esp_is_not_self() {
        assert!(!skip_self(
            "\\EFI\\BOOT\\BOOTX64.EFI",
            Some("\\EFI\\BOOT\\BOOTX64.EFI"),
            false
        ));
        assert!(skip_self(
            "\\efi\\boot\\bootx64.efi",
            Some("\\EFI\\BOOT\\BOOTX64.EFI"),
            true
        ));
    }

    #[test]
    fn missing_self_path_preserves_other_volumes_and_known_loaders() {
        assert!(skip_self("\\EFI\\BOOT\\BOOTX64.EFI", None, true));
        assert!(!skip_self("\\EFI\\BOOT\\BOOTX64.EFI", None, false));
        assert!(!skip_self("\\EFI\\ubuntu\\shimx64.efi", None, true));
    }
}
