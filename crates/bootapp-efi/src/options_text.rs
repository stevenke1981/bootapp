//! Render untrusted UTF-16LE load options as one bounded trace line.

use alloc::string::String;

/// Maximum UTF-16 code units rendered; longer options end with `...`.
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub const MAX_OPTION_UNITS: usize = 1024;

/// Decode little-endian UTF-16 up to the first NUL (or the end), replacing
/// unpaired surrogates and control characters with `?` so a hostile option
/// string cannot inject extra trace lines. An odd trailing byte is ignored.
#[cfg_attr(not(target_os = "uefi"), allow(dead_code))]
pub fn load_options_text(bytes: &[u8]) -> String {
    let mut units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0);
    let mut text: String = char::decode_utf16(units.by_ref().take(MAX_OPTION_UNITS))
        .map(|item| match item {
            Ok(c) if !c.is_control() => c,
            _ => '?',
        })
        .collect();
    if units.next().is_some() {
        text.push_str("...");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn decodes_until_nul() {
        let mut bytes = utf16("console=ttyS0 initrd=\\a.img");
        bytes.extend_from_slice(&utf16("ignored"));
        assert_eq!(load_options_text(&bytes), "console=ttyS0 initrd=\\a.img");
        assert_eq!(load_options_text(&utf16("")), "");
        assert_eq!(load_options_text(&[]), "");
    }

    #[test]
    fn controls_surrogates_and_length_are_bounded() {
        assert_eq!(load_options_text(&utf16("a\r\nB")), "a??B");
        assert_eq!(load_options_text(&[0x00, 0xD8, 0x41, 0x00]), "?A");
        let long: String = core::iter::repeat_n('x', MAX_OPTION_UNITS + 5).collect();
        let text = load_options_text(&utf16(&long));
        assert_eq!(text.len(), MAX_OPTION_UNITS + 3);
        assert!(text.ends_with("..."));
        let exact: String = core::iter::repeat_n('y', MAX_OPTION_UNITS).collect();
        assert_eq!(load_options_text(&utf16(&exact)), exact);
    }
}
