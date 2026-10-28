#![no_std]

extern crate alloc;

pub mod detection;

use alloc::{string::String, vec::Vec};
use core::fmt;

pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub const MAX_ENTRIES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X86,
    X64,
    Aarch64,
}

impl Architecture {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "x86" => Some(Self::X86),
            "x64" => Some(Self::X64),
            "aarch64" => Some(Self::Aarch64),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x64",
            Self::Aarch64 => "aarch64",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub path: String,
    pub arch: Option<Architecture>,
    pub options: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub timeout: u32,
    pub default: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// One-based source line; zero indicates a document-level error.
    pub line: usize,
    pub message: &'static str,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

fn invalid(line: usize, message: &'static str) -> ConfigError {
    ConfigError { line, message }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Validate an absolute path within a firmware filesystem volume.
/// This does not resolve files or establish file identity (FAT aliases included).
pub fn validate_efi_path(path: &str) -> bool {
    if !path.starts_with('\\')
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return false;
    }
    let mut components = path[1..].split('\\').peekable();
    while let Some(component) = components.next() {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with([' ', '.'])
        {
            return false;
        }
        if components.peek().is_none() {
            return component.len() > 4
                && component
                    .get(component.len() - 4..)
                    .is_some_and(|extension| extension.eq_ignore_ascii_case(".efi"));
        }
    }
    false
}

struct PendingEntry {
    entry: Entry,
    seen: u8,
    line: usize,
}

impl PendingEntry {
    fn new(id: &str, line: usize) -> Self {
        Self {
            entry: Entry {
                id: String::from(id),
                title: String::new(),
                path: String::new(),
                arch: None,
                options: String::new(),
            },
            seen: 0,
            line,
        }
    }

    fn finish(self) -> Result<Entry, ConfigError> {
        if self.entry.title.is_empty() {
            return Err(invalid(self.line, "entry requires a nonempty title"));
        }
        if !validate_efi_path(&self.entry.path) {
            return Err(invalid(
                self.line,
                "entry requires a valid absolute EFI path",
            ));
        }
        Ok(self.entry)
    }

    fn set(&mut self, key: &str, value: &str, line: usize) -> Result<(), ConfigError> {
        let bit = match key {
            "title" => 1,
            "path" => 2,
            "arch" => 4,
            "options" => 8,
            _ => return Err(invalid(line, "unknown entry key")),
        };
        if self.seen & bit != 0 {
            return Err(invalid(line, "duplicate entry key"));
        }
        self.seen |= bit;
        match key {
            "title" => self.entry.title = String::from(value),
            "path" => self.entry.path = String::from(value),
            "arch" => {
                self.entry.arch = Some(
                    Architecture::parse(value)
                        .ok_or_else(|| invalid(line, "unknown architecture"))?,
                );
            }
            "options" => self.entry.options = String::from(value),
            _ => unreachable!(),
        }
        Ok(())
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(invalid(0, "configuration exceeds 64 KiB"));
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut timeout = 5;
        let mut default = None;
        let mut globals_seen = 0u8;
        let mut entries: Vec<Entry> = Vec::new();
        let mut pending: Option<PendingEntry> = None;
        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            // CRLF is accepted; other control characters could affect the console
            // or silently truncate firmware strings, including in comments.
            if raw
                .trim_end_matches('\r')
                .chars()
                .any(|c| c.is_control() && c != '\t')
            {
                return Err(invalid(line_number, "control character in configuration"));
            }
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                if let Some(previous) = pending.take() {
                    entries.push(previous.finish()?);
                }
                let id = line
                    .strip_prefix("[entry ")
                    .and_then(|s| s.strip_suffix(']'))
                    .ok_or_else(|| invalid(line_number, "expected [entry ID] section"))?;
                if !valid_id(id) {
                    return Err(invalid(line_number, "invalid entry ID"));
                }
                if entries.iter().any(|entry| entry.id == id) {
                    return Err(invalid(line_number, "duplicate entry ID"));
                }
                if entries.len() >= MAX_ENTRIES {
                    return Err(invalid(line_number, "too many entries"));
                }
                pending = Some(PendingEntry::new(id, line_number));
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| invalid(line_number, "expected key=value"))?;
            let key = key.trim();
            let value = value.trim();
            if value.chars().any(char::is_control) {
                return Err(invalid(line_number, "control character in value"));
            }
            if let Some(entry) = pending.as_mut() {
                entry.set(key, value, line_number)?;
            } else {
                let bit = match key {
                    "timeout" => 1,
                    "default" => 2,
                    _ => return Err(invalid(line_number, "unknown global key")),
                };
                if globals_seen & bit != 0 {
                    return Err(invalid(line_number, "duplicate global key"));
                }
                globals_seen |= bit;
                match key {
                    "timeout" => {
                        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(invalid(
                                line_number,
                                "timeout must be an integer from 0 to 3600",
                            ));
                        }
                        timeout = value
                            .parse::<u32>()
                            .ok()
                            .filter(|&n| n <= 3600)
                            .ok_or_else(|| {
                                invalid(line_number, "timeout must be an integer from 0 to 3600")
                            })?;
                    }
                    "default" => {
                        if !valid_id(value) {
                            return Err(invalid(line_number, "invalid default entry ID"));
                        }
                        default = Some(String::from(value));
                    }
                    _ => unreachable!(),
                }
            }
        }
        if let Some(last) = pending {
            entries.push(last.finish()?);
        }
        let first = entries
            .first()
            .ok_or_else(|| invalid(0, "configuration requires at least one entry"))?;
        let default = default.unwrap_or_else(|| first.id.clone());
        if !entries.iter().any(|entry| entry.id == default) {
            return Err(invalid(0, "default references an unknown entry"));
        }
        Ok(Self {
            timeout,
            default,
            entries,
        })
    }

    pub fn entries_for(&self, arch: Architecture) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.arch.is_none() || entry.arch == Some(arch))
            .collect()
    }

    /// Index within `entries_for(arch)`, falling back to its first compatible
    /// entry when the configured default belongs to a different architecture.
    pub fn default_index(&self, arch: Architecture) -> Option<usize> {
        let entries = self.entries_for(arch);
        if entries.is_empty() {
            None
        } else {
            Some(
                entries
                    .iter()
                    .position(|entry| entry.id == self.default)
                    .unwrap_or(0),
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent {
    Up,
    Down,
    Tick,
    Enter,
    CancelTimeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    Boot(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    len: usize,
    selected: usize,
    remaining: Option<u32>,
}

impl MenuState {
    pub fn new(len: usize, default_index: usize, timeout: u32) -> Self {
        Self {
            len,
            selected: if default_index < len {
                default_index
            } else {
                0
            },
            remaining: if len == 0 { None } else { Some(timeout) },
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn remaining(&self) -> Option<u32> {
        self.remaining
    }

    pub fn handle(&mut self, event: MenuEvent) -> MenuAction {
        if event != MenuEvent::Tick {
            self.remaining = None;
        }
        if self.len == 0 {
            return MenuAction::None;
        }
        match event {
            MenuEvent::Tick => {
                if let Some(seconds) = self.remaining {
                    if seconds <= 1 {
                        self.remaining = None;
                        return MenuAction::Boot(self.selected);
                    }
                    self.remaining = Some(seconds - 1);
                }
            }
            MenuEvent::Up => {
                self.selected = if self.selected == 0 {
                    self.len - 1
                } else {
                    self.selected - 1
                };
            }
            MenuEvent::Down => {
                self.selected = if self.selected == self.len - 1 {
                    0
                } else {
                    self.selected + 1
                };
            }
            MenuEvent::Enter => return MenuAction::Boot(self.selected),
            MenuEvent::CancelTimeout => {}
        }
        MenuAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    const ENTRY: &str =
        "[entry windows]\ntitle=Windows\npath=\\EFI\\Microsoft\\Boot\\bootmgfw.efi\n";

    #[test]
    fn parse_defaults_and_utf8_crlf() {
        let cfg = Config::parse("# comment\r\n[entry os]\r\ntitle=作業系統\r\npath=\\EFI\\系統\\BOOT.EFI\r\noptions=quiet foo=bar\r\n").unwrap();
        assert_eq!(cfg.timeout, 5);
        assert_eq!(cfg.default, "os");
        assert_eq!(cfg.entries[0].title, "作業系統");
        assert_eq!(cfg.entries[0].options, "quiet foo=bar");
        assert_eq!(cfg.entries[0].arch, None);
    }

    #[test]
    fn architecture_filter_and_default_index_use_filtered_positions() {
        let cfg = Config::parse("default=arm\n[entry ia32]\ntitle=IA32\npath=\\ia32.efi\narch=x86\n[entry arm]\ntitle=Arm\npath=\\arm.efi\narch=aarch64\n[entry common]\ntitle=Common\npath=\\common.efi\n").unwrap();
        assert_eq!(cfg.entries_for(Architecture::X64)[0].id, "common");
        assert_eq!(cfg.default_index(Architecture::X64), Some(0));
        assert_eq!(cfg.default_index(Architecture::Aarch64), Some(0));
        let only = Config::parse(&format!("{ENTRY}arch=x64\n")).unwrap();
        assert_eq!(only.default_index(Architecture::X86), None);
        assert!(only.entries_for(Architecture::X86).is_empty());
        for arch in [Architecture::X86, Architecture::X64, Architecture::Aarch64] {
            assert_eq!(Architecture::parse(arch.as_str()), Some(arch));
        }
        assert_eq!(Architecture::parse("amd64"), None);
    }

    #[test]
    fn rejects_unknown_duplicate_and_missing_fields() {
        let bad = [
            String::new(),
            String::from("[entry os]\n"),
            String::from("[entry os]\ntitle=OS\n"),
            String::from("[entry os]\npath=\\os.efi\n"),
            format!("{ENTRY}title=Again\n"),
            format!("{ENTRY}path=\\another.efi\n"),
            format!("{ENTRY}arch=x64\narch=x86\n"),
            format!("{ENTRY}options=\noptions=\n"),
            format!("{ENTRY}typo=value\n"),
            format!("oops=x\n{ENTRY}"),
            format!("timeout=1\ntimeout=2\n{ENTRY}"),
            format!("default=windows\ndefault=windows\n{ENTRY}"),
            format!("default=missing\n{ENTRY}"),
            format!("{ENTRY}{ENTRY}"),
            format!("{ENTRY}arch=arm\n"),
            format!("{ENTRY}timeout=1\n"),
        ];
        for text in bad {
            assert!(Config::parse(&text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn validates_ids_sections_and_values() {
        for id in ["", "a b", "a/b", "../os", "作業系統", "x]suffix"] {
            assert!(Config::parse(&format!("[entry {id}]\ntitle=OS\npath=\\os.efi\n")).is_err());
        }
        assert!(Config::parse(&format!(
            "[entry {}]\ntitle=OS\npath=\\os.efi",
            "a".repeat(65)
        ))
        .is_err());
        for text in [
            "[entry os] trailing",
            "[os]",
            "title",
            "[entry os",
            "[entry os]\ntitle=\npath=\\os.efi",
        ] {
            assert!(Config::parse(text).is_err());
        }
        for control in ['\0', '\u{1b}', '\u{7f}'] {
            assert!(Config::parse(&format!("{ENTRY}options=a{control}b")).is_err());
        }
        assert!(Config::parse(&format!("{ENTRY}options=a\tb")).is_err());
    }

    #[test]
    fn timeout_bounds_and_invalid_numbers() {
        for value in [
            "",
            "-1",
            "+1",
            "3601",
            "4294967296",
            "five",
            "1.0",
            "1 # comment",
        ] {
            assert!(
                Config::parse(&format!("timeout={value}\n{ENTRY}")).is_err(),
                "{value}"
            );
        }
        for value in [0, 1, 3600] {
            assert_eq!(
                Config::parse(&format!("timeout={value}\n{ENTRY}"))
                    .unwrap()
                    .timeout,
                value
            );
        }
    }

    #[test]
    fn accepts_leading_utf8_bom() {
        let cfg = Config::parse(&format!("\u{feff}timeout=1\n{ENTRY}")).unwrap();
        assert_eq!(cfg.timeout, 1);
        assert!(Config::parse(&format!("{ENTRY}\u{feff}options=x\n")).is_err());
    }

    #[test]
    fn enforces_document_and_entry_limits() {
        assert!(Config::parse(&" ".repeat(MAX_CONFIG_BYTES + 1)).is_err());
        let padded = format!("{ENTRY}{}", " ".repeat(MAX_CONFIG_BYTES - ENTRY.len()));
        assert!(Config::parse(&padded).is_ok());
        let mut text = String::new();
        for index in 0..MAX_ENTRIES {
            text.push_str(&format!("[entry e{index}]\ntitle=OS\npath=\\os.efi\n"));
        }
        assert_eq!(Config::parse(&text).unwrap().entries.len(), MAX_ENTRIES);
        text.push_str("[entry extra]\ntitle=OS\npath=\\os.efi\n");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_paths_outside_absolute_efi_contract() {
        for path in [
            "",
            "os.efi",
            "/os.efi",
            "\\",
            "\\.efi",
            "\\..\\os.efi",
            "\\.\\os.efi",
            "\\EFI\\\\os.efi",
            "\\\\server\\os.efi",
            "\\EFI/os.efi",
            "C:\\os.efi",
            "\\os.efi:other",
            "\\os.exe",
            "\\os.efi\\",
            "\\EFI.\\os.efi",
            "\\EFI \\os.efi",
            "\\bad\0.efi",
            "\\bad\u{1b}.efi",
            "\\*.efi",
            "\\?.efi",
        ] {
            assert!(!validate_efi_path(path), "accepted {path:?}");
        }
        for path in [
            "\\os.efi",
            "\\EFI\\BOOT\\BOOTX64.EFI",
            "\\EFI\\系統\\kernel.efi",
            "\\a.b\\a.efi",
        ] {
            assert!(validate_efi_path(path), "rejected {path:?}");
        }
        // An arbitrary UTF-8 boundary near the suffix must not panic.
        assert!(!validate_efi_path("\\😀"));
    }

    #[test]
    fn error_reports_source_line() {
        let error = Config::parse("# comment\nwrong=value\n").unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(format!("{error}"), "line 2: unknown global key");
    }

    #[test]
    fn countdown_boots_exactly_once_and_zero_is_immediate_tick() {
        let mut state = MenuState::new(3, 1, 2);
        assert_eq!(state.remaining(), Some(2));
        assert_eq!(state.handle(MenuEvent::Tick), MenuAction::None);
        assert_eq!(state.remaining(), Some(1));
        assert_eq!(state.handle(MenuEvent::Tick), MenuAction::Boot(1));
        assert_eq!(state.remaining(), None);
        assert_eq!(state.handle(MenuEvent::Tick), MenuAction::None);
        let mut zero = MenuState::new(1, 0, 0);
        assert_eq!(zero.handle(MenuEvent::Tick), MenuAction::Boot(0));
        assert_eq!(zero.handle(MenuEvent::Tick), MenuAction::None);
    }

    #[test]
    fn key_interaction_cancels_timeout_and_navigation_wraps() {
        for event in [
            MenuEvent::Up,
            MenuEvent::Down,
            MenuEvent::Enter,
            MenuEvent::CancelTimeout,
        ] {
            let mut state = MenuState::new(3, 0, 1);
            let action = state.handle(event);
            assert_eq!(state.remaining(), None);
            assert_eq!(state.handle(MenuEvent::Tick), MenuAction::None);
            if event == MenuEvent::Enter {
                assert_eq!(action, MenuAction::Boot(0));
            }
        }
        let mut state = MenuState::new(3, 0, 5);
        state.handle(MenuEvent::Up);
        assert_eq!(state.selected(), 2);
        state.handle(MenuEvent::Down);
        assert_eq!(state.selected(), 0);
        state.handle(MenuEvent::Down);
        assert_eq!(state.selected(), 1);
        assert_eq!(state.handle(MenuEvent::Enter), MenuAction::Boot(1));
    }

    #[test]
    fn empty_and_invalid_initial_selection_are_safe() {
        let mut empty = MenuState::new(0, usize::MAX, 0);
        for event in [
            MenuEvent::Tick,
            MenuEvent::Up,
            MenuEvent::Down,
            MenuEvent::Enter,
            MenuEvent::CancelTimeout,
        ] {
            assert_eq!(empty.handle(event), MenuAction::None);
            assert_eq!(empty.selected(), 0);
            assert_eq!(empty.remaining(), None);
        }
        assert_eq!(MenuState::new(2, 2, 5).selected(), 0);
        let mut huge = MenuState::new(usize::MAX, usize::MAX - 1, 1);
        huge.handle(MenuEvent::Down);
        assert_eq!(huge.selected(), 0);
    }
}
