//! GRUB 2 `grub.cfg` compatibility subset (CONTRACT — see docs/GRUB.md).
//!
//! Parses a bounded, documented subset of GRUB script into the same `Config`
//! model as bootapp.cfg. Nothing is executed; unsupported constructs are
//! rejected with a line number instead of being silently ignored.

use crate::{
    valid_id, validate_volume_path, Config, Entry, VolumeSelector, MAX_CONFIG_BYTES, MAX_ENTRIES,
    TIMEOUT_DISABLED,
};
use alloc::{format, string::String, vec::Vec};
use core::fmt;
use core::iter::Peekable;
use core::str::Chars;

/// Firmware location of a GRUB-style config, on the manager's own volume.
/// Used only when `\EFI\BOOTAPP\bootapp.cfg` is absent.
pub const GRUB_CONFIG_PATH: &str = "\\EFI\\BOOTAPP\\grub.cfg";

/// Maximum `submenu` nesting depth.
pub const MAX_SUBMENU_DEPTH: usize = 8;

const DEFAULT_TIMEOUT: u32 = 5;
const MAX_TIMEOUT: u32 = 3600;
const MAX_WORD_BYTES: usize = 4096;
const MAX_OPTION_BYTES: usize = 4096;
const MAX_VARIABLES: usize = 256;
const MAX_TITLE_CHARS: usize = 256;
/// Longest raw `--id` text kept for `set default=` matching.
const MAX_RAW_ID_CHARS: usize = 256;

/// Commands accepted and ignored with a warning: module loading, display and
/// terminal setup, sound and delays. None of them changes what BootApp boots.
const NO_EFFECT_COMMANDS: &[&str] = &[
    "insmod",
    "load_video",
    "terminal_input",
    "terminal_output",
    "terminal",
    "font",
    "loadfont",
    "gfxmode",
    "echo",
    "background_image",
    "play",
    "sleep",
    "serial",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrubError {
    /// One-based source line; zero indicates a document-level error.
    pub line: usize,
    pub message: &'static str,
}

impl fmt::Display for GrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(self.message)
        } else {
            write!(f, "grub.cfg line {}: {}", self.line, self.message)
        }
    }
}

/// A construct that was accepted but has no effect (e.g. `insmod`, `saved` default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrubWarning {
    pub line: usize,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrubConfig {
    pub config: Config,
    pub warnings: Vec<GrubWarning>,
}

/// Parse a GRUB config subset. Same 64 KiB / 128-entry limits as `Config::parse`.
pub fn parse(text: &str) -> Result<GrubConfig, GrubError> {
    if text.len() > MAX_CONFIG_BYTES {
        return fail(0, "configuration exceeds 64 KiB");
    }
    // CRLF is accepted; any other control character is rejected, as in bootapp.cfg.
    let source = text.replace("\r\n", "\n");
    let mut line = 1;
    for c in source.chars() {
        if c == '\n' {
            line += 1;
        } else if c != '\t' && c.is_control() {
            return fail(line, "control character in configuration");
        }
    }
    let tokens = lex(&source)?;
    let mut parser = Parser::new(tokens);
    let tree = parser.parse_block(Ctx::Top, 0, "", 1)?;
    parser.finish(tree)
}

fn fail<T>(line: usize, message: &'static str) -> Result<T, GrubError> {
    Err(GrubError { line, message })
}

// ---------------------------------------------------------------------------
// Lexer: words, separators and braces. Variable references are kept unexpanded
// so that `set` commands executed later in the file affect them in order.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Var(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    parts: Vec<Part>,
    line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(Word),
    /// Newline or `;`.
    Sep(usize),
    /// Standalone `{`.
    Open(usize),
    /// Standalone `}`.
    Close(usize),
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    line: usize,
    tokens: Vec<Tok>,
    word: Option<Word>,
    word_bytes: usize,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn lex(source: &str) -> Result<Vec<Tok>, GrubError> {
    let mut lx = Lexer {
        chars: source.chars().peekable(),
        line: 1,
        tokens: Vec::new(),
        word: None,
        word_bytes: 0,
    };
    while let Some(c) = lx.chars.next() {
        match c {
            ' ' | '\t' => lx.end_word(),
            '\n' => {
                lx.end_word();
                lx.tokens.push(Tok::Sep(lx.line));
                lx.line += 1;
            }
            ';' => {
                lx.end_word();
                lx.tokens.push(Tok::Sep(lx.line));
            }
            '#' if lx.word.is_none() => {
                // Comment: skip to end of line, leaving the newline for the separator.
                while let Some(&next) = lx.chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    lx.chars.next();
                }
            }
            '\'' => lx.single_quoted()?,
            '"' => lx.double_quoted()?,
            '\\' => {
                let escaped = match lx.chars.next() {
                    Some(next) => next,
                    None => return fail(lx.line, "dangling backslash"),
                };
                if escaped == '\n' {
                    return fail(lx.line, "backslash before newline is not supported");
                }
                lx.push_char(escaped)?;
            }
            '$' => match lx.variable()? {
                Some(name) => lx.push_var(name)?,
                None => lx.push_char('$')?,
            },
            '{' | '}' if lx.word.is_none() && lx.at_word_boundary() => {
                let token = if c == '{' {
                    Tok::Open(lx.line)
                } else {
                    Tok::Close(lx.line)
                };
                lx.tokens.push(token);
            }
            _ => lx.push_char(c)?,
        }
    }
    lx.end_word();
    Ok(lx.tokens)
}

impl Lexer<'_> {
    fn at_word_boundary(&mut self) -> bool {
        matches!(self.chars.peek(), None | Some(' ' | '\t' | '\n' | ';'))
    }

    fn current_word(&mut self) -> &mut Word {
        let line = self.line;
        self.word.get_or_insert_with(|| Word {
            parts: Vec::new(),
            line,
        })
    }

    fn add_bytes(&mut self, count: usize) -> Result<(), GrubError> {
        self.word_bytes += count;
        if self.word_bytes > MAX_WORD_BYTES {
            return fail(self.line, "word exceeds 4096 bytes");
        }
        Ok(())
    }

    fn push_char(&mut self, c: char) -> Result<(), GrubError> {
        self.add_bytes(c.len_utf8())?;
        let word = self.current_word();
        if !matches!(word.parts.last(), Some(Part::Lit(_))) {
            word.parts.push(Part::Lit(String::new()));
        }
        if let Some(Part::Lit(text)) = word.parts.last_mut() {
            text.push(c);
        }
        Ok(())
    }

    fn push_var(&mut self, name: String) -> Result<(), GrubError> {
        self.add_bytes(name.len() + 3)?;
        self.current_word().parts.push(Part::Var(name));
        Ok(())
    }

    fn end_word(&mut self) {
        if let Some(word) = self.word.take() {
            self.tokens.push(Tok::Word(word));
        }
        self.word_bytes = 0;
    }

    /// Reads a variable reference after `$`. `Ok(None)` means a literal `$`.
    fn variable(&mut self) -> Result<Option<String>, GrubError> {
        match self.chars.peek().copied() {
            Some('{') => {
                self.chars.next();
                let mut name = String::new();
                loop {
                    match self.chars.next() {
                        Some('}') => break,
                        Some(c) if is_name_char(c) => name.push(c),
                        Some(_) => return fail(self.line, "invalid variable name"),
                        None => return fail(self.line, "unterminated ${ variable reference"),
                    }
                }
                if name.is_empty() {
                    return fail(self.line, "empty variable name");
                }
                Ok(Some(name))
            }
            Some(c) if is_name_char(c) => {
                let mut name = String::new();
                while let Some(&next) = self.chars.peek() {
                    if !is_name_char(next) {
                        break;
                    }
                    name.push(next);
                    self.chars.next();
                }
                Ok(Some(name))
            }
            _ => Ok(None),
        }
    }

    fn single_quoted(&mut self) -> Result<(), GrubError> {
        let start = self.line;
        self.current_word();
        loop {
            match self.chars.next() {
                Some('\'') => return Ok(()),
                Some('\n') => {
                    self.line += 1;
                    self.push_char('\n')?;
                }
                Some(c) => self.push_char(c)?,
                None => return fail(start, "unterminated single quote"),
            }
        }
    }

    fn double_quoted(&mut self) -> Result<(), GrubError> {
        let start = self.line;
        self.current_word();
        loop {
            match self.chars.next() {
                Some('"') => return Ok(()),
                Some('\\') => match self.chars.peek().copied() {
                    Some(next @ ('"' | '\\' | '$')) => {
                        self.chars.next();
                        self.push_char(next)?;
                    }
                    _ => self.push_char('\\')?,
                },
                Some('$') => match self.variable()? {
                    Some(name) => self.push_var(name)?,
                    None => self.push_char('$')?,
                },
                Some('\n') => {
                    self.line += 1;
                    self.push_char('\n')?;
                }
                Some(c) => self.push_char(c)?,
                None => return fail(start, "unterminated double quote"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parser: commands, blocks and BootApp entries.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Term {
    Sep,
    Open,
    Close,
    Eof,
}

struct Cmd {
    words: Vec<Word>,
    term: Term,
    line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Top,
    Submenu,
    Entry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loader {
    Chainloader,
    Linux,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Var {
    name: String,
    value: String,
    /// Set by `search --set`; literal `set` values clear it.
    selector: Option<VolumeSelector>,
}

enum Node {
    Entry(usize),
    Submenu(Vec<Node>),
}

struct EntryState {
    title: String,
    id_request: Option<String>,
    line: usize,
    loader: Option<Loader>,
    path: String,
    volume: Option<VolumeSelector>,
    /// Quoted kernel arguments, in order.
    args: Vec<String>,
    /// `initrd=` options; a later `initrd` command replaces this list.
    initrds: Vec<String>,
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
    vars: Vec<Var>,
    timeout: u32,
    default_spec: Option<(String, usize)>,
    entries: Vec<Entry>,
    /// The raw `--id` text of each entry (parallel to `entries`), kept so that
    /// `set default=` can name grub-mkconfig ids that BootApp ids reject.
    raw_ids: Vec<Option<String>>,
    warnings: Vec<GrubWarning>,
    entry: Option<EntryState>,
}

impl Parser {
    fn new(tokens: Vec<Tok>) -> Self {
        Self {
            tokens,
            pos: 0,
            vars: alloc::vec![Var {
                name: String::from("menuentry_id_option"),
                value: String::from("--id"),
                selector: None,
            }],
            timeout: DEFAULT_TIMEOUT,
            default_spec: None,
            entries: Vec::new(),
            raw_ids: Vec::new(),
            warnings: Vec::new(),
            entry: None,
        }
    }

    fn warn(&mut self, line: usize, message: &'static str) {
        self.warnings.push(GrubWarning { line, message });
    }

    /// Reads words up to the next separator, brace or end of input.
    fn read_command(&mut self) -> Option<Cmd> {
        if self.pos >= self.tokens.len() {
            return None;
        }
        let mut words = Vec::new();
        let mut line = 0;
        loop {
            let Some(token) = self.tokens.get(self.pos) else {
                return Some(Cmd {
                    words,
                    term: Term::Eof,
                    line,
                });
            };
            let (term, term_line) = match token {
                Tok::Word(word) => {
                    if words.is_empty() {
                        line = word.line;
                    }
                    words.push(word.clone());
                    self.pos += 1;
                    continue;
                }
                Tok::Sep(at) => (Term::Sep, *at),
                Tok::Open(at) => (Term::Open, *at),
                Tok::Close(at) => (Term::Close, *at),
            };
            self.pos += 1;
            if words.is_empty() {
                line = term_line;
            }
            return Some(Cmd { words, term, line });
        }
    }

    fn parse_block(
        &mut self,
        ctx: Ctx,
        depth: usize,
        prefix: &str,
        open_line: usize,
    ) -> Result<Vec<Node>, GrubError> {
        let mut nodes = Vec::new();
        loop {
            let Some(cmd) = self.read_command() else {
                if ctx == Ctx::Top {
                    return Ok(nodes);
                }
                return fail(open_line, "menu block is missing its closing }");
            };
            if cmd.words.is_empty() {
                if cmd.term == Term::Open {
                    return fail(cmd.line, "unexpected {");
                }
            } else {
                self.dispatch(ctx, depth, prefix, &cmd, &mut nodes)?;
            }
            if cmd.term == Term::Close {
                if ctx == Ctx::Top {
                    return fail(cmd.line, "unexpected }");
                }
                return Ok(nodes);
            }
        }
    }

    fn dispatch(
        &mut self,
        ctx: Ctx,
        depth: usize,
        prefix: &str,
        cmd: &Cmd,
        nodes: &mut Vec<Node>,
    ) -> Result<(), GrubError> {
        let name = self.expand(&cmd.words[0])?;
        if name == "menuentry" || name == "submenu" {
            if ctx == Ctx::Entry {
                return fail(cmd.line, "menu entries cannot be nested inside menuentry");
            }
            return self.menu(depth, prefix, name == "submenu", cmd, nodes);
        }
        // The command itself is checked first so that unsupported constructs such
        // as `function f {` report their own reason.
        if ctx == Ctx::Entry {
            self.entry_command(&name, cmd)?;
        } else {
            self.block_command(ctx, &name, cmd)?;
        }
        if cmd.term == Term::Open {
            return fail(cmd.line, "{ is only valid after menuentry or submenu");
        }
        Ok(())
    }

    fn menu(
        &mut self,
        depth: usize,
        prefix: &str,
        is_submenu: bool,
        cmd: &Cmd,
        nodes: &mut Vec<Node>,
    ) -> Result<(), GrubError> {
        let line = cmd.line;
        if cmd.term != Term::Open {
            return fail(
                line,
                if is_submenu {
                    "submenu requires a { block"
                } else {
                    "menuentry requires a { block"
                },
            );
        }
        let args = self.expand_all(&cmd.words)?;
        let raw_title = args.get(1).ok_or(GrubError {
            line,
            message: "menu entry requires a title",
        })?;
        let joined = if prefix.is_empty() {
            String::from(raw_title.trim())
        } else {
            format!("{prefix} > {}", raw_title.trim())
        };
        let full_title = checked_title(&joined, line)?;

        let mut id_request: Option<String> = None;
        let mut index = 2;
        while index < args.len() {
            let (option, inline) = split_option(&args[index]);
            match option {
                "--unrestricted" if inline.is_none() => index += 1,
                "--id" | "--class" | "--users" | "--hotkey" => {
                    let (value, consumed) = option_value(&args, index, inline, line)?;
                    if option == "--id" {
                        id_request = Some(String::from(value));
                    }
                    index += consumed;
                }
                _ => return fail(line, "unsupported menu option"),
            }
        }

        if is_submenu {
            let child_depth = depth + 1;
            if child_depth > MAX_SUBMENU_DEPTH {
                return fail(line, "submenu nesting exceeds the depth limit");
            }
            // A submenu body runs only when entered, so it is scoped like an entry:
            // variables it sets do not leak into the rest of the file.
            let saved_vars = self.vars.clone();
            let children = self.parse_block(Ctx::Submenu, child_depth, &full_title, line);
            self.vars = saved_vars;
            nodes.push(Node::Submenu(children?));
            return Ok(());
        }

        if self.entries.len() >= MAX_ENTRIES {
            return fail(line, "configuration has more than 128 entries");
        }
        let saved_vars = self.vars.clone();
        self.entry = Some(EntryState {
            title: full_title,
            id_request,
            line,
            loader: None,
            path: String::new(),
            volume: None,
            args: Vec::new(),
            initrds: Vec::new(),
        });
        self.parse_block(Ctx::Entry, depth, "", line)?;
        let Some(state) = self.entry.take() else {
            return fail(line, "internal error: missing menuentry state");
        };
        self.vars = saved_vars;
        self.finish_entry(state, nodes)
    }

    fn finish_entry(&mut self, state: EntryState, nodes: &mut Vec<Node>) -> Result<(), GrubError> {
        let line = state.line;
        if state.loader.is_none() {
            return fail(
                line,
                "menuentry requires exactly one chainloader or linux command",
            );
        }
        let mut parts = state.args;
        parts.extend(state.initrds);
        let options = parts.join(" ");
        if options.len() > MAX_OPTION_BYTES {
            return fail(line, "menuentry options exceed 4096 bytes");
        }
        let (id, raw_id) = self.choose_id(state.id_request.as_deref(), line);
        let volume = state.volume.unwrap_or(VolumeSelector::Own);
        self.entries.push(Entry {
            id,
            title: state.title,
            path: state.path,
            arch: None,
            options,
            volume,
        });
        self.raw_ids.push(raw_id);
        nodes.push(Node::Entry(self.entries.len() - 1));
        Ok(())
    }

    /// Returns the entry id and the raw `--id` text (kept for `set default=`).
    /// A valid, unused `--id` is used as the id. Otherwise the id is generated as
    /// `grub-<n>` from the 1-based flattened entry index, skipping ids already taken.
    fn choose_id(&mut self, requested: Option<&str>, line: usize) -> (String, Option<String>) {
        let mut raw_id = None;
        if let Some(text) = requested {
            if text.chars().count() > MAX_RAW_ID_CHARS {
                self.warn(
                    line,
                    "menu --id is longer than 256 characters; a generated id was used",
                );
            } else {
                raw_id = Some(String::from(text));
                if valid_id(text) && !self.id_taken(text) {
                    return (String::from(text), raw_id);
                }
                self.warn(
                    line,
                    "menu --id is invalid or duplicated; a generated id was used",
                );
            }
        }
        let mut number = self.entries.len() + 1;
        loop {
            let candidate = format!("grub-{number}");
            if !self.id_taken(&candidate) {
                return (candidate, raw_id);
            }
            number += 1;
        }
    }

    fn id_taken(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    fn block_command(&mut self, ctx: Ctx, name: &str, cmd: &Cmd) -> Result<(), GrubError> {
        match name {
            "set" => self.cmd_set(ctx, cmd),
            "unset" => self.cmd_unset(cmd),
            "export" => self.cmd_export(cmd),
            "search" => self.cmd_search(cmd),
            _ => self.common_command(name, cmd.line),
        }
    }

    fn entry_command(&mut self, name: &str, cmd: &Cmd) -> Result<(), GrubError> {
        match name {
            "set" => self.cmd_set(Ctx::Entry, cmd),
            "unset" => self.cmd_unset(cmd),
            "export" => self.cmd_export(cmd),
            "search" => self.cmd_search(cmd),
            "chainloader" => self.cmd_chainloader(cmd),
            "linux" | "linuxefi" => self.cmd_linux(cmd),
            "initrd" | "initrdefi" => self.cmd_initrd(cmd),
            "boot" => Ok(()),
            _ => self.common_command(name, cmd.line),
        }
    }

    /// Commands valid in any context that are either ignored or rejected.
    fn common_command(&mut self, name: &str, line: usize) -> Result<(), GrubError> {
        let message = match name {
            "if" | "then" | "else" | "elif" | "fi" => {
                "conditional statements (if/then/else) are not supported"
            }
            "for" | "while" => "loops are not supported",
            "function" => "GRUB functions are not supported",
            "source" | "." | "configfile" => "loading other configuration files is not supported",
            "normal" => "GRUB normal mode is not supported",
            "chainloader" | "linux" | "linuxefi" | "initrd" | "initrdefi" | "boot" => {
                "command is only allowed inside menuentry"
            }
            _ if NO_EFFECT_COMMANDS.contains(&name) => {
                self.warn(line, "command has no effect in BootApp and was ignored");
                return Ok(());
            }
            _ => "unsupported or unknown GRUB command",
        };
        fail(line, message)
    }

    /// `set NAME=VALUE`. `timeout` and `default` are global and may be set at top
    /// level only; inside a submenu they are ignored with a warning. When the value
    /// is exactly one variable reference (`set root=$dev`), the variable's search
    /// selector is copied, so the value still names a search result.
    fn cmd_set(&mut self, ctx: Ctx, cmd: &Cmd) -> Result<(), GrubError> {
        let line = cmd.line;
        let args = self.expand_all(&cmd.words)?;
        let assignment = match args.as_slice() {
            [_, assignment] => assignment.as_str(),
            _ => return fail(line, "set requires exactly one NAME=VALUE argument"),
        };
        let Some((name, value)) = assignment.split_once('=') else {
            return fail(line, "set requires NAME=VALUE");
        };
        if !valid_var_name(name) {
            return fail(line, "invalid variable name");
        }
        let global = matches!(name, "timeout" | "default");
        if ctx == Ctx::Submenu && global {
            self.warn(
                line,
                "timeout and default can only be set outside submenus; ignored",
            );
            return Ok(());
        }
        let selector = match sole_variable(&cmd.words[1]) {
            Some(source) => self
                .vars
                .iter()
                .find(|var| var.name == source)
                .and_then(|var| var.selector.clone()),
            None => None,
        };
        self.store_var(name, value, selector, line)?;
        if ctx == Ctx::Top {
            match name {
                "timeout" => self.timeout = parse_timeout(value, line)?,
                "default" => self.default_spec = Some((String::from(value), line)),
                _ => {}
            }
        }
        Ok(())
    }

    fn cmd_unset(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let args = self.expand_all(&cmd.words)?;
        match args.as_slice() {
            [_, name] if valid_var_name(name) => {
                self.vars.retain(|var| var.name != *name);
                Ok(())
            }
            _ => fail(cmd.line, "unset requires exactly one variable name"),
        }
    }

    fn cmd_export(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let args = self.expand_all(&cmd.words)?;
        match args.as_slice() {
            [_, name] if valid_var_name(name) => Ok(()),
            _ => fail(cmd.line, "export requires exactly one variable name"),
        }
    }

    fn store_var(
        &mut self,
        name: &str,
        value: &str,
        selector: Option<VolumeSelector>,
        line: usize,
    ) -> Result<(), GrubError> {
        if value.len() > MAX_WORD_BYTES {
            return fail(line, "variable value exceeds 4096 bytes");
        }
        let others: usize = self
            .vars
            .iter()
            .filter(|var| var.name != name)
            .map(|var| var.value.len())
            .sum();
        if others + value.len() > MAX_CONFIG_BYTES {
            return fail(line, "variables exceed 64 KiB");
        }
        if let Some(var) = self.vars.iter_mut().find(|var| var.name == name) {
            var.value = String::from(value);
            var.selector = selector;
            return Ok(());
        }
        if self.vars.len() >= MAX_VARIABLES {
            return fail(line, "too many variables");
        }
        self.vars.push(Var {
            name: String::from(name),
            value: String::from(value),
            selector,
        });
        Ok(())
    }

    /// `search [-n] [--set[=VAR]] (--file|-f|--fs-uuid|-u|--label|-l) [--hint* X] KEY`
    ///
    /// As in GRUB, the identifier options are flags and the key is the trailing
    /// positional argument.
    fn cmd_search(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let line = cmd.line;
        let args = self.expand_all(&cmd.words)?;
        let mut target = String::from("root");
        let mut identifier: Option<&str> = None;
        let mut key: Option<&str> = None;
        let mut index = 1;
        while index < args.len() {
            let arg = args[index].as_str();
            let (option, inline) = split_option(arg);
            index += 1;
            match option {
                "-n" | "--no-floppy" if inline.is_none() => {}
                "--set" => target = String::from(inline.unwrap_or("root")),
                "-s" if inline.is_none() => target = String::from("root"),
                "--file" | "-f" | "--fs-uuid" | "-u" | "--label" | "-l" if inline.is_none() => {
                    if identifier.is_some() {
                        return fail(line, "search accepts only one identifier option");
                    }
                    identifier = Some(option);
                }
                _ if option.starts_with("--hint") => {
                    let (_, consumed) = option_value(&args, index - 1, inline, line)?;
                    index += consumed - 1;
                }
                _ if !arg.starts_with('-') => {
                    if key.is_some() {
                        return fail(line, "search takes exactly one key");
                    }
                    key = Some(arg);
                }
                _ => return fail(line, "unsupported search option or argument"),
            }
        }
        let Some(identifier) = identifier else {
            return fail(line, "search requires --file, --fs-uuid or --label");
        };
        let Some(key) = key else {
            return fail(line, "search requires a value");
        };
        let value = String::from(key);
        if !valid_var_name(&target) {
            return fail(line, "invalid search variable name");
        }
        let (selector, stored) = match identifier {
            "--file" | "-f" => {
                let path = value.replace('/', "\\");
                if !validate_volume_path(&path) {
                    return fail(line, "search --file requires an absolute path");
                }
                (VolumeSelector::File(path.clone()), path)
            }
            "--fs-uuid" | "-u" => {
                let upper = value.to_ascii_uppercase();
                if !is_fat_serial(&upper) {
                    return fail(line, "search --fs-uuid requires an XXXX-XXXX volume ID");
                }
                (VolumeSelector::FsUuid(upper.clone()), upper)
            }
            _ => {
                if value.is_empty() {
                    return fail(line, "search --label requires a label");
                }
                (VolumeSelector::Label(value.clone()), value)
            }
        };
        self.store_var(&target, &stored, Some(selector), line)
    }

    fn cmd_chainloader(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let line = cmd.line;
        self.entry_mut(line)?;
        if cmd.words.len() != 2 {
            return fail(line, "chainloader requires exactly one path");
        }
        if self.expand(&cmd.words[1])?.starts_with('+') {
            return fail(line, "chainloader +N block lists are not supported");
        }
        let (path, volume) = self.resolve_path(&cmd.words[1], line)?;
        self.attach_loader(Loader::Chainloader, path, volume, Vec::new(), line)
    }

    fn cmd_linux(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let line = cmd.line;
        if cmd.words.len() < 2 {
            return fail(line, "linux requires a kernel path");
        }
        self.entry_mut(line)?;
        let (path, volume) = self.resolve_path(&cmd.words[1], line)?;
        // A word that expands to nothing (an unset variable) vanishes, as in GRUB.
        let mut args = Vec::new();
        for word in &cmd.words[2..] {
            let text = self.expand(word)?;
            if !text.is_empty() {
                args.push(quote_kernel_arg(&text));
            }
        }
        self.attach_loader(Loader::Linux, path, volume, args, line)
    }

    fn cmd_initrd(&mut self, cmd: &Cmd) -> Result<(), GrubError> {
        let line = cmd.line;
        if self.entry_mut(line)?.loader != Some(Loader::Linux) {
            return fail(line, "initrd requires a preceding linux command");
        }
        if cmd.words.len() < 2 {
            return fail(line, "initrd requires at least one path");
        }
        // Each `initrd` command replaces the previous list (GRUB behaviour).
        let mut options = Vec::new();
        for word in &cmd.words[1..] {
            let (path, volume) = self.resolve_path(word, line)?;
            if path.contains(' ') {
                return fail(line, "initrd path must not contain spaces");
            }
            self.check_volume(&volume, line)?;
            options.push(format!("initrd={path}"));
        }
        self.entry_mut(line)?.initrds = options;
        Ok(())
    }

    fn entry_mut(&mut self, line: usize) -> Result<&mut EntryState, GrubError> {
        match self.entry.as_mut() {
            Some(state) => Ok(state),
            None => fail(line, "command is only allowed inside menuentry"),
        }
    }

    /// Every path of a menuentry must be on the same volume, because the entry
    /// has a single volume and Linux resolves `initrd=` on the kernel's volume.
    fn check_volume(&mut self, volume: &VolumeSelector, line: usize) -> Result<(), GrubError> {
        let state = self.entry_mut(line)?;
        match &state.volume {
            Some(existing) if existing != volume => {
                fail(line, "all paths in a menuentry must be on the same volume")
            }
            Some(_) => Ok(()),
            None => {
                state.volume = Some(volume.clone());
                Ok(())
            }
        }
    }

    fn attach_loader(
        &mut self,
        loader: Loader,
        path: String,
        volume: VolumeSelector,
        args: Vec<String>,
        line: usize,
    ) -> Result<(), GrubError> {
        if self.entry_mut(line)?.loader.is_some() {
            return fail(line, "menuentry already has a loader command");
        }
        self.check_volume(&volume, line)?;
        let state = self.entry_mut(line)?;
        state.loader = Some(loader);
        state.path = path;
        state.args.extend(args);
        Ok(())
    }

    /// Converts a GRUB path word to an absolute backslash path and its volume.
    /// `($root)` and `($VAR)` select the volume of a search result; a bare
    /// `(device)` name is rejected because GRUB device numbering is not mapped.
    fn resolve_path(
        &self,
        word: &Word,
        line: usize,
    ) -> Result<(String, VolumeSelector), GrubError> {
        let (volume, text) = match device_prefix(&word.parts) {
            Some((name, tail, rest)) => {
                let volume = self.prefix_volume(name, line)?;
                let mut text = String::from(tail);
                text.push_str(&self.expand_parts(rest, line)?);
                (volume, text)
            }
            None => {
                let text = self.expand(word)?;
                if text.starts_with('(') {
                    return fail(line, "GRUB device names are not mapped; use search");
                }
                (self.root_volume(line)?, text)
            }
        };
        if !text.starts_with('/') {
            return fail(line, "GRUB paths must be absolute");
        }
        let path = text.replace('/', "\\");
        if !validate_volume_path(&path) {
            return fail(line, "path is not valid for BootApp");
        }
        Ok((path, volume))
    }

    fn prefix_volume(&self, name: &str, line: usize) -> Result<VolumeSelector, GrubError> {
        if name == "root" {
            return self.root_volume(line);
        }
        match self.vars.iter().find(|var| var.name == name) {
            Some(Var {
                selector: Some(selector),
                ..
            }) => Ok(selector.clone()),
            Some(_) => fail(line, "device prefix variable must be set by search"),
            None => fail(line, "device prefix variable is unset"),
        }
    }

    /// Volume selected by `root`: unset means the boot manager's own volume, a
    /// search result is used directly, and a literal GRUB device name is rejected.
    fn root_volume(&self, line: usize) -> Result<VolumeSelector, GrubError> {
        match self.vars.iter().find(|var| var.name == "root") {
            None => Ok(VolumeSelector::Own),
            Some(var) => match &var.selector {
                Some(selector) => Ok(selector.clone()),
                None if var.value.is_empty() => Ok(VolumeSelector::Own),
                None => fail(line, "root is not a search result; use search"),
            },
        }
    }

    fn var_value(&self, name: &str) -> &str {
        self.vars
            .iter()
            .find(|var| var.name == name)
            .map_or("", |var| var.value.as_str())
    }

    fn expand(&self, word: &Word) -> Result<String, GrubError> {
        self.expand_parts(&word.parts, word.line)
    }

    fn expand_parts(&self, parts: &[Part], line: usize) -> Result<String, GrubError> {
        let mut out = String::new();
        for part in parts {
            match part {
                Part::Lit(text) => out.push_str(text),
                Part::Var(name) => out.push_str(self.var_value(name)),
            }
            if out.len() > MAX_WORD_BYTES {
                return fail(line, "word exceeds 4096 bytes");
            }
        }
        if out.chars().any(char::is_control) {
            return fail(line, "control character in configuration");
        }
        Ok(out)
    }

    fn expand_all(&self, words: &[Word]) -> Result<Vec<String>, GrubError> {
        words.iter().map(|word| self.expand(word)).collect()
    }

    fn finish(mut self, tree: Vec<Node>) -> Result<GrubConfig, GrubError> {
        if self.entries.is_empty() {
            return fail(0, "configuration requires at least one entry");
        }
        let index = match self.default_spec.take() {
            None => 0,
            Some((value, line)) if value.is_empty() || value == "saved" => {
                self.warn(
                    line,
                    "default 'saved' or an empty value selects the first entry",
                );
                0
            }
            Some((value, line)) => find_default(&tree, &self.entries, &self.raw_ids, &value)
                .ok_or(GrubError {
                    line,
                    message: "default references an unknown entry",
                })?,
        };
        let default = self.entries[index].id.clone();
        Ok(GrubConfig {
            config: Config {
                timeout: self.timeout,
                default,
                entries: self.entries,
            },
            warnings: self.warnings,
        })
    }
}

/// The variable name when a word is `NAME=$VAR` or `NAME=${VAR}` with nothing
/// else after the reference, as in `set root=$dev`.
fn sole_variable(word: &Word) -> Option<&str> {
    match word.parts.as_slice() {
        [Part::Lit(text), Part::Var(name)] if text.ends_with('=') => Some(name.as_str()),
        _ => None,
    }
}

/// Matches the leading `(` `$VAR` `)` device prefix of a path word, returning the
/// variable name, any literal text after `)`, and the remaining parts.
fn device_prefix(parts: &[Part]) -> Option<(&str, &str, &[Part])> {
    match parts {
        [Part::Lit(open), Part::Var(name), Part::Lit(close), rest @ ..] if open.as_str() == "(" => {
            let tail = close.strip_prefix(')')?;
            Some((name.as_str(), tail, rest))
        }
        _ => None,
    }
}

/// Quotes one kernel argument the way GRUB's loader command line does: an
/// argument containing whitespace or `"` is wrapped in double quotes, and inside
/// the quotes each `\` and each `"` is preceded by a backslash. Other arguments
/// are emitted unchanged.
fn quote_kernel_arg(arg: &str) -> String {
    if !arg.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
        return String::from(arg);
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Index, then BootApp id, then the raw `--id` text, then the flattened title.
fn find_default(
    tree: &[Node],
    entries: &[Entry],
    raw_ids: &[Option<String>],
    value: &str,
) -> Option<usize> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit() || b == b'>') {
        let segments: Vec<&str> = value.split('>').collect();
        if let Some(index) = walk_default(tree, &segments) {
            return Some(index);
        }
    }
    entries
        .iter()
        .position(|entry| entry.id == value)
        .or_else(|| raw_ids.iter().position(|raw| raw.as_deref() == Some(value)))
        .or_else(|| entries.iter().position(|entry| entry.title == value))
}

/// Follows `N>M>...` indices through submenus; the path must end on an entry.
fn walk_default(nodes: &[Node], segments: &[&str]) -> Option<usize> {
    let (first, rest) = segments.split_first()?;
    let position: usize = first.parse().ok()?;
    match nodes.get(position)? {
        Node::Entry(index) => {
            if rest.is_empty() {
                Some(*index)
            } else {
                None
            }
        }
        Node::Submenu(children) => {
            if rest.is_empty() {
                None
            } else {
                walk_default(children, rest)
            }
        }
    }
}

fn valid_var_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn parse_timeout(value: &str, line: usize) -> Result<u32, GrubError> {
    if value == "-1" {
        return Ok(TIMEOUT_DISABLED);
    }
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        if let Some(seconds) = value.parse::<u32>().ok().filter(|&n| n <= MAX_TIMEOUT) {
            return Ok(seconds);
        }
    }
    fail(line, "timeout must be -1 or an integer from 0 to 3600")
}

fn checked_title(raw: &str, line: usize) -> Result<String, GrubError> {
    let title = raw.trim();
    if title.is_empty() {
        return fail(line, "menu title must not be empty");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return fail(line, "menu title exceeds 256 characters");
    }
    Ok(String::from(title))
}

/// Splits `--name=value` into its parts; other words are returned whole.
fn split_option(arg: &str) -> (&str, Option<&str>) {
    if arg.starts_with('-') {
        if let Some(eq) = arg.find('=') {
            return (&arg[..eq], Some(&arg[eq + 1..]));
        }
    }
    (arg, None)
}

/// Value of an option given inline (`--name=value`) or as the next argument.
/// Returns the value and how many arguments were consumed.
fn option_value<'a>(
    args: &'a [String],
    index: usize,
    inline: Option<&'a str>,
    line: usize,
) -> Result<(&'a str, usize), GrubError> {
    if let Some(value) = inline {
        return Ok((value, 1));
    }
    match args.get(index + 1) {
        Some(value) => Ok((value.as_str(), 2)),
        None => fail(line, "option requires a value"),
    }
}

/// FAT volume serial `XXXX-XXXX`.
fn is_fat_serial(text: &str) -> bool {
    text.len() == 9
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 4 {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};

    fn parse_ok(text: &str) -> GrubConfig {
        match parse(text) {
            Ok(config) => config,
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    fn parse_err(text: &str) -> GrubError {
        match parse(text) {
            Ok(_) => panic!("expected an error for:\n{text}"),
            Err(error) => error,
        }
    }

    fn entry<'a>(config: &'a GrubConfig, id: &str) -> &'a Entry {
        config
            .config
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("entry present")
    }

    fn titles(config: &GrubConfig) -> Vec<String> {
        config
            .config
            .entries
            .iter()
            .map(|entry| entry.title.clone())
            .collect()
    }

    const DEBIAN: &str = "\
set timeout=5
set default=\"0\"
insmod part_gpt
insmod ext2
search --no-floppy --fs-uuid --set=root ABCD-1234
menuentry 'Debian GNU/Linux' --class debian --class gnu-linux --class os $menuentry_id_option 'gnu-linux-simple-ABCD' {
    insmod gzio
    echo 'Loading Linux 6.12 ...'
    linux /vmlinuz-6.12 root=UUID=1111-2222 ro quiet
    initrd /initrd.img-6.12
}
";

    const WINDOWS: &str = "\
search --file --set=root /EFI/Microsoft/Boot/bootmgfw.efi
menuentry 'Windows Boot Manager' --class windows --id windows-boot {
    chainloader /EFI/Microsoft/Boot/bootmgfw.efi
}
";

    const SUBMENU: &str = "\
set default=\"1>0\"
menuentry 'First' { chainloader /A.efi }
submenu 'More' {
    menuentry 'Second' { chainloader /B.efi }
    submenu 'Deeper' {
        menuentry 'Third' { chainloader /C.efi }
    }
}
menuentry 'Last' { chainloader /D.efi }
";

    #[test]
    fn debian_style_entry_maps_to_linux_with_initrd() {
        let config = parse_ok(DEBIAN);
        assert_eq!(config.config.timeout, 5);
        assert_eq!(config.config.default, "gnu-linux-simple-ABCD");
        assert_eq!(config.config.entries.len(), 1);
        assert_eq!(
            config.config.entries[0],
            Entry {
                id: "gnu-linux-simple-ABCD".to_string(),
                title: "Debian GNU/Linux".to_string(),
                path: "\\vmlinuz-6.12".to_string(),
                arch: None,
                options: "root=UUID=1111-2222 ro quiet initrd=\\initrd.img-6.12".to_string(),
                volume: VolumeSelector::FsUuid("ABCD-1234".to_string()),
            }
        );
        assert!(config
            .warnings
            .iter()
            .any(|w| w.message.contains("no effect")));
    }

    #[test]
    fn windows_chainloader_uses_file_search_volume() {
        let config = parse_ok(WINDOWS);
        let entry = entry(&config, "windows-boot");
        assert_eq!(entry.title, "Windows Boot Manager");
        assert_eq!(entry.path, "\\EFI\\Microsoft\\Boot\\bootmgfw.efi");
        assert_eq!(entry.options, "");
        assert_eq!(
            entry.volume,
            VolumeSelector::File("\\EFI\\Microsoft\\Boot\\bootmgfw.efi".to_string())
        );
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn timeout_bounds_and_errors() {
        assert_eq!(
            parse_ok("set timeout=-1\nmenuentry a { chainloader /a.efi }")
                .config
                .timeout,
            TIMEOUT_DISABLED
        );
        assert_eq!(
            parse_ok("set timeout=0\nmenuentry a { chainloader /a.efi }")
                .config
                .timeout,
            0
        );
        assert_eq!(
            parse_ok("set timeout=3600\nmenuentry a { chainloader /a.efi }")
                .config
                .timeout,
            3600
        );
        assert_eq!(
            parse_ok("menuentry a { chainloader /a.efi }")
                .config
                .timeout,
            5
        );
        for bad in [
            "set timeout=3601",
            "set timeout=abc",
            "set timeout=",
            "set timeout=-2",
            "set timeout=+5",
        ] {
            let text = format!("{bad}\nmenuentry a {{ chainloader /a.efi }}");
            assert_eq!(parse_err(&text).line, 1, "{bad}");
        }
    }

    #[test]
    fn default_by_index_path_id_title_and_fallbacks() {
        const BODY: &str = "menuentry 'First' { chainloader /A.efi }\n\
                            submenu 'More' { menuentry 'Second' { chainloader /B.efi } }\n";
        let submenu = parse_ok(SUBMENU);
        assert_eq!(submenu.config.default, entry(&submenu, "grub-2").id);
        assert_eq!(entry(&submenu, "grub-2").title, "More > Second");

        let by_index = parse_ok("set default=2\nmenuentry a { chainloader /a.efi }\nmenuentry b { chainloader /b.efi }\nmenuentry c { chainloader /c.efi }");
        assert_eq!(by_index.config.default, "grub-3");

        let by_id = parse_ok("set default=mine\nmenuentry a --id mine { chainloader /a.efi }\nmenuentry b { chainloader /b.efi }");
        assert_eq!(by_id.config.default, "mine");

        let by_title = parse_ok("set default='Second Entry'\nmenuentry a { chainloader /a.efi }\nmenuentry 'Second Entry' { chainloader /b.efi }");
        assert_eq!(by_title.config.default, "grub-2");

        let saved = parse_ok("set default=saved\nmenuentry a { chainloader /a.efi }\nmenuentry b { chainloader /b.efi }");
        assert_eq!(saved.config.default, "grub-1");
        assert_eq!(saved.warnings.len(), 1);

        let empty = parse_ok("set default=\nmenuentry a { chainloader /a.efi }");
        assert_eq!(empty.config.default, "grub-1");
        assert_eq!(empty.warnings.len(), 1);

        let unknown = parse_err("set default=nope\nmenuentry a { chainloader /a.efi }");
        assert_eq!(unknown.line, 1);
        assert_eq!(unknown.message, "default references an unknown entry");

        // Index pointing at a submenu, or a path that ends on a submenu or out of range, is unknown.
        assert!(parse(&format!("set default=1\n{BODY}")).is_err());
        assert!(parse(&format!("set default=1>9\n{BODY}")).is_err());
        assert!(parse(&format!("set default=1>0\n{BODY}")).is_ok());
    }

    #[test]
    fn submenu_flattening_titles_and_ids() {
        let config = parse_ok(SUBMENU);
        assert_eq!(
            titles(&config),
            vec!["First", "More > Second", "More > Deeper > Third", "Last"]
        );
        let ids: Vec<&str> = config
            .config
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["grub-1", "grub-2", "grub-3", "grub-4"]);
    }

    #[test]
    fn submenu_depth_limit_is_enforced() {
        let build = |levels: usize| {
            let mut text = String::new();
            for level in 0..levels {
                text.push_str(&format!("submenu 'L{level}' {{\n"));
            }
            text.push_str("menuentry leaf { chainloader /leaf.efi }\n");
            for _ in 0..levels {
                text.push_str("}\n");
            }
            text
        };
        assert_eq!(parse_ok(&build(MAX_SUBMENU_DEPTH)).config.entries.len(), 1);
        let error = parse_err(&build(MAX_SUBMENU_DEPTH + 1));
        assert_eq!(error.message, "submenu nesting exceeds the depth limit");
    }

    #[test]
    fn quoting_escaping_and_expansion() {
        // Single quotes are literal; double quotes honor \" \\ \$ and expand variables.
        let config = parse_ok(
            "set raw='a $b \\n \"q\"'\n\
             set dq=\"x\\\"y\\\\z\\$w\"\n\
             set base=/boot\n\
             set dir=\"${base}/efi\"\n\
             set joined=$base/x\n\
             set empty=''\n\
             menuentry 'k' { linux ${dir}/k $raw $dq $empty $joined $unset end }\n",
        );
        let entry = &config.config.entries[0];
        assert_eq!(entry.path, "\\boot\\efi\\k");
        // Arguments with spaces or quotes are double-quoted, with \ and " escaped.
        assert_eq!(entry.options, r#""a $b \\n \"q\"" "x\"y\\z$w" /boot/x end"#);
    }

    #[test]
    fn quoted_newlines_and_literal_dollar_in_quotes() {
        let config = parse_ok("set price=\"$ 5\"\nmenuentry 'T' --id ok { chainloader /ok.efi }");
        assert_eq!(config.config.entries[0].id, "ok");
        // An unknown `${` reference is a syntax error, not a literal.
        assert_eq!(
            parse_err("set a=${oops\nmenuentry a { chainloader /a.efi }").message,
            "invalid variable name"
        );
        assert_eq!(
            parse_err("set a=${bad-name}\nmenuentry a { chainloader /a.efi }").message,
            "invalid variable name"
        );
    }

    #[test]
    fn variable_expansion_feeds_options_and_titles() {
        let config = parse_ok(
            "set kernel=/vmlinuz\nset opts=\"quiet splash\"\n\
             menuentry 'Kernel' { linux $kernel $opts root=LABEL=x; }\n",
        );
        let entry = &config.config.entries[0];
        assert_eq!(entry.path, "\\vmlinuz");
        assert_eq!(entry.options, "\"quiet splash\" root=LABEL=x");
    }

    #[test]
    fn unquoted_backslash_and_quotes_produce_single_words() {
        let config = parse_ok(
            "set esc=a\\ b\nset dq=\"x\\\"y\"\nmenuentry 'k' { linux /k a\\ b \"c d\" }\n",
        );
        assert_eq!(config.config.entries[0].options, "\"a b\" \"c d\"");
    }

    #[test]
    fn comments_semicolons_and_braces() {
        let config = parse_ok(
            "# leading comment\n\
             set hash=b#c   # trailing comment\n\
             menuentry 'one' { chainloader /one.efi; }\n\
             menuentry 'two' { chainloader /two.efi ; }\n\
             menuentry 'three' { chainloader /three.efi }\n",
        );
        assert_eq!(titles(&config), vec!["one", "two", "three"]);
        // A brace attached to a word is part of that word, so the header is invalid.
        assert_eq!(
            parse_err("menuentry 'x' {chainloader /a.efi }").message,
            "menuentry requires a { block"
        );
    }

    #[test]
    fn unterminated_quotes_report_their_start_line() {
        let error = parse_err("set a=1\nmenuentry 'open {\n chainloader /a.efi\n");
        assert_eq!(error.line, 2);
        assert_eq!(error.message, "unterminated single quote");
        let error = parse_err("set a=\"open\nmenuentry x { chainloader /a.efi }\n");
        assert_eq!(error.line, 1);
        assert_eq!(error.message, "unterminated double quote");
        assert_eq!(
            parse_err("menuentry x { chainloader /a.efi }\necho \\").line,
            2
        );
    }

    #[test]
    fn oversize_document_is_rejected() {
        let text = format!("# {}", "x".repeat(MAX_CONFIG_BYTES));
        assert_eq!(parse_err(&text).line, 0);
        let ok = format!(
            "# {}\nmenuentry a {{ chainloader /a.efi }}",
            "x".repeat(MAX_CONFIG_BYTES - 100)
        );
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn entry_limit_allows_128_and_rejects_129() {
        let mut text = String::new();
        for i in 0..MAX_ENTRIES {
            text.push_str(&format!("menuentry 'e{i}' {{ chainloader /e{i}.efi }}\n"));
        }
        assert_eq!(parse_ok(&text).config.entries.len(), MAX_ENTRIES);
        text.push_str("menuentry 'extra' { chainloader /x.efi }\n");
        let error = parse_err(&text);
        assert_eq!(error.line, MAX_ENTRIES + 1);
        assert_eq!(error.message, "configuration has more than 128 entries");
    }

    #[test]
    fn crlf_input_matches_lf_input() {
        let lf = parse_ok(DEBIAN);
        let crlf = parse_ok(&DEBIAN.replace('\n', "\r\n"));
        assert_eq!(lf.config, crlf.config);
        assert_eq!(lf.warnings, crlf.warnings);
        // A lone CR is not part of CRLF and is rejected.
        assert_eq!(
            parse_err("set a=1\rmenuentry a { chainloader /a.efi }").message,
            "control character in configuration"
        );
    }

    #[test]
    fn unsupported_constructs_are_rejected_with_their_line() {
        let cases = [
            (
                "if [ -f /x ]; then",
                "conditional statements (if/then/else) are not supported",
            ),
            (
                "then",
                "conditional statements (if/then/else) are not supported",
            ),
            ("for x in a b; do", "loops are not supported"),
            ("while true; do", "loops are not supported"),
            ("function f {", "GRUB functions are not supported"),
            (
                "source /x.cfg",
                "loading other configuration files is not supported",
            ),
            (
                ". /x.cfg",
                "loading other configuration files is not supported",
            ),
            (
                "configfile /x.cfg",
                "loading other configuration files is not supported",
            ),
            ("normal", "GRUB normal mode is not supported"),
            ("frobnicate", "unsupported or unknown GRUB command"),
            (
                "chainloader /x.efi",
                "command is only allowed inside menuentry",
            ),
        ];
        for (line, message) in cases {
            let text = format!("set timeout=1\n{line}\nmenuentry a {{ chainloader /a.efi }}\n");
            let error = parse_err(&text);
            assert_eq!(error.line, 2, "{line}");
            assert_eq!(error.message, message, "{line}");
        }
    }

    #[test]
    fn menu_syntax_errors() {
        assert_eq!(
            parse_err("menuentry 'a'\nchainloader /a.efi").message,
            "menuentry requires a { block"
        );
        assert_eq!(
            parse_err("submenu 'a'\nmenuentry b { chainloader /b.efi }").message,
            "submenu requires a { block"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi }\n}").message,
            "unexpected }"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi }\n{").message,
            "unexpected {"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi\n").message,
            "menu block is missing its closing }"
        );
        assert_eq!(
            parse_err("menuentry a { menuentry b { chainloader /b.efi } }").message,
            "menu entries cannot be nested inside menuentry"
        );
        assert_eq!(
            parse_err("menuentry '' { chainloader /a.efi }").message,
            "menu title must not be empty"
        );
        assert_eq!(
            parse_err("menuentry a --bogus { chainloader /a.efi }").message,
            "unsupported menu option"
        );
        assert_eq!(
            parse_err("").message,
            "configuration requires at least one entry"
        );
        assert_eq!(
            parse_err("set timeout=5\n").message,
            "configuration requires at least one entry"
        );
        assert_eq!(
            parse_err("submenu 'only' { }").message,
            "configuration requires at least one entry"
        );
    }

    #[test]
    fn loader_rules_are_enforced() {
        assert_eq!(
            parse_err("menuentry a { }").message,
            "menuentry requires exactly one chainloader or linux command"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi\n chainloader /b.efi }").message,
            "menuentry already has a loader command"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi\n linux /k }").message,
            "menuentry already has a loader command"
        );
        assert_eq!(
            parse_err("menuentry a { initrd /i.img\n linux /k }").message,
            "initrd requires a preceding linux command"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi\n initrd /i.img }").message,
            "initrd requires a preceding linux command"
        );
        // A second initrd command replaces the first list.
        let two_initrd = parse_ok("menuentry a { linux /k ro\n initrd /i1 /i2\n initrd /i3 }");
        assert_eq!(two_initrd.config.entries[0].options, "ro initrd=\\i3");
        let one_command = parse_ok("menuentry a { linux /k ro\n initrd /i1 /i2 }");
        assert_eq!(
            one_command.config.entries[0].options,
            "ro initrd=\\i1 initrd=\\i2"
        );
        let bare = parse_ok("menuentry a { linux /k\n boot }");
        assert_eq!(bare.config.entries[0].options, "");
        let efi = parse_ok("menuentry a { linuxefi /k\n initrdefi /i }");
        assert_eq!(efi.config.entries[0].options, "initrd=\\i");
        assert_eq!(
            parse_err("menuentry a { linux }").message,
            "linux requires a kernel path"
        );
        assert_eq!(
            parse_err("menuentry a { linux /k\n initrd }").message,
            "initrd requires at least one path"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader }").message,
            "chainloader requires exactly one path"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a.efi /b.efi }").message,
            "chainloader requires exactly one path"
        );
    }

    #[test]
    fn device_names_blocklists_and_relative_paths_are_rejected() {
        assert_eq!(
            parse_err("menuentry a { chainloader (hd0,gpt1)/EFI/x.efi }").message,
            "GRUB device names are not mapped; use search"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader (hd0) }").message,
            "GRUB device names are not mapped; use search"
        );
        assert_eq!(
            parse_err("menuentry a { linux (cd)/k }").message,
            "GRUB device names are not mapped; use search"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader +1 }").message,
            "chainloader +N block lists are not supported"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader EFI/x.efi }").message,
            "GRUB paths must be absolute"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /a/../b.efi }").message,
            "path is not valid for BootApp"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader /x:y.efi }").message,
            "path is not valid for BootApp"
        );
        assert_eq!(
            parse_err("menuentry a { chainloader ($nothere)/x.efi }").message,
            "device prefix variable is unset"
        );
    }

    #[test]
    fn root_and_device_prefixes_select_search_volumes() {
        let prefixed = parse_ok(
            "search --fs-uuid --set=root ABCD-1234\n\
             menuentry a { linux ($root)/vmlinuz root=/dev/sda1 }\n",
        );
        assert_eq!(prefixed.config.entries[0].path, "\\vmlinuz");
        assert_eq!(
            prefixed.config.entries[0].volume,
            VolumeSelector::FsUuid("ABCD-1234".to_string())
        );
        let other_var = parse_ok(
            "search --set=disk --label BOOTDISK\n\
             menuentry a { chainloader ($disk)/EFI/x.efi }\n",
        );
        assert_eq!(
            other_var.config.entries[0].volume,
            VolumeSelector::Label("BOOTDISK".to_string())
        );
        assert_eq!(
            parse_err("set other=hello\nmenuentry a { chainloader ($other)/x.efi }").message,
            "device prefix variable must be set by search"
        );
        // Without any search, $root means the boot manager's own volume.
        assert_eq!(
            parse_ok("menuentry a { chainloader ($root)/x.efi }")
                .config
                .entries[0]
                .volume,
            VolumeSelector::Own
        );
    }

    #[test]
    fn literal_root_is_rejected_when_used() {
        // Any non-empty literal that is not a search result is an error when used.
        assert_eq!(
            parse_err("set root=hd0,gpt1\nmenuentry a { chainloader /a.efi }").message,
            "root is not a search result; use search"
        );
        assert_eq!(
            parse_err("set root=plain\nmenuentry a { chainloader /a.efi }").message,
            "root is not a search result; use search"
        );
        // Setting root literally after a search replaces the selector, so it is an error.
        assert!(parse(
            "search --set=root --label BOOT\nset root=hd0\nmenuentry a { chainloader /a.efi }"
        )
        .is_err());
        // An empty root means the boot manager's own volume.
        let config = parse_ok("set root=\nmenuentry a { chainloader /a.efi }");
        assert_eq!(config.config.entries[0].volume, VolumeSelector::Own);
    }

    #[test]
    fn variable_copied_from_search_keeps_its_volume() {
        // Regression: `set root=$dev` must keep the search selector of $dev.
        let top = parse_ok(
            "search --fs-uuid --set=dev ABCD-1234\n\
             set root=$dev\n\
             menuentry a { linux /vmlinuz }\n",
        );
        assert_eq!(
            top.config.entries[0].volume,
            VolumeSelector::FsUuid("ABCD-1234".to_string())
        );
        let inside = parse_ok(
            "menuentry a {\n\
               search --file --set=dev /EFI/x.efi\n\
               set root=${dev}\n\
               chainloader /EFI/x.efi\n\
             }\n",
        );
        assert_eq!(
            inside.config.entries[0].volume,
            VolumeSelector::File("\\EFI\\x.efi".to_string())
        );
        let label = parse_ok(
            "search --set=dev --label BOOT\nset root=\"$dev\"\nmenuentry a { chainloader /a.efi }",
        );
        assert_eq!(
            label.config.entries[0].volume,
            VolumeSelector::Label("BOOT".to_string())
        );
        // A value that is not exactly one variable reference loses the selector.
        assert!(parse(
            "search --set=dev --label BOOT\nset root=$dev/x\nmenuentry a { chainloader /a.efi }"
        )
        .is_err());
    }

    #[test]
    fn submenu_body_does_not_set_globals_or_leak_variables() {
        // Regression: timeout and default inside a submenu are ignored with a warning.
        let config = parse_ok(
            "submenu 'S' {\n\
               set timeout=0\n\
               set default=b\n\
               menuentry b { chainloader /b.efi }\n\
             }\n\
             menuentry a { chainloader /a.efi }\n",
        );
        assert_eq!(config.config.timeout, 5);
        assert_eq!(config.config.default, "grub-1");
        let lines: Vec<usize> = config.warnings.iter().map(|w| w.line).collect();
        assert_eq!(lines, vec![2, 3]);
        // A search inside a submenu does not affect later top-level entries.
        let leak = parse_ok(
            "submenu 'S' {\n\
               search --fs-uuid --set=root AAAA-1111\n\
               menuentry b { chainloader /b.efi }\n\
             }\n\
             menuentry a { chainloader /a.efi }\n",
        );
        assert_eq!(leak.config.entries[1].volume, VolumeSelector::Own);
        assert_eq!(
            leak.config.entries[0].volume,
            VolumeSelector::FsUuid("AAAA-1111".to_string())
        );
    }

    #[test]
    fn raw_grub_mkconfig_id_is_usable_as_default() {
        // Regression: grub-mkconfig ids contain dots, which BootApp ids reject.
        let raw = "gnulinux-6.1.0-13-amd64-advanced-0123abcd";
        let config = parse_ok(&format!(
            "set default={raw}\nmenuentry 'Debian' --id {raw} {{ chainloader /a.efi }}\n"
        ));
        assert_eq!(config.config.entries[0].id, "grub-1");
        assert_eq!(config.config.default, "grub-1");
        assert!(config.warnings.iter().any(|w| w.message.contains("--id")));
        // Raw ids longer than 256 characters are not kept for matching.
        let long = "x".repeat(MAX_RAW_ID_CHARS + 1);
        let error = parse_err(&format!(
            "set default={long}\nmenuentry 'D' --id {long} {{ chainloader /a.efi }}\n"
        ));
        assert_eq!(error.message, "default references an unknown entry");
    }

    #[test]
    fn kernel_arguments_with_spaces_are_quoted() {
        // Regression: GRUB's loader command line keeps a quoted argument together.
        let config = parse_ok("menuentry a { linux /vmlinuz 'acpi_osi=Windows 2020' quiet }");
        assert_eq!(
            config.config.entries[0].options,
            "\"acpi_osi=Windows 2020\" quiet"
        );
        assert_eq!(quote_kernel_arg("plain=1"), "plain=1");
        assert_eq!(quote_kernel_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_kernel_arg("c:\\x y"), "\"c:\\\\x y\"");
    }

    #[test]
    fn kernel_and_initrd_must_share_a_volume() {
        let error = parse_err(
            "search --fs-uuid --set=root AAAA-1111\n\
             menuentry a {\n\
               linux /vmlinuz\n\
               search --fs-uuid --set=root BBBB-2222\n\
               initrd /initrd.img\n\
             }\n",
        );
        assert_eq!(error.line, 5);
        assert_eq!(
            error.message,
            "all paths in a menuentry must be on the same volume"
        );
        // The same volume set again is fine.
        let same = parse_ok(
            "search --fs-uuid --set=root AAAA-1111\n\
             menuentry a { linux /vmlinuz\n search --fs-uuid --set=root aaaa-1111\n initrd /i }\n",
        );
        assert_eq!(same.config.entries[0].options, "initrd=\\i");
    }

    #[test]
    fn search_argument_validation() {
        assert_eq!(
            parse_err("search --set=root\nmenuentry a { chainloader /a.efi }").message,
            "search requires --file, --fs-uuid or --label"
        );
        assert_eq!(
            parse_err("search --fs-uuid ABCD1234\nmenuentry a { chainloader /a.efi }").message,
            "search --fs-uuid requires an XXXX-XXXX volume ID"
        );
        assert_eq!(
            parse_err("search --fs-uuid 1111-22223\nmenuentry a { chainloader /a.efi }").message,
            "search --fs-uuid requires an XXXX-XXXX volume ID"
        );
        assert_eq!(
            parse_err("search --file EFI/x.efi\nmenuentry a { chainloader /a.efi }").message,
            "search --file requires an absolute path"
        );
        assert_eq!(
            parse_err("search --label\nmenuentry a { chainloader /a.efi }").message,
            "search requires a value"
        );
        assert_eq!(
            parse_err("search --label '' \nmenuentry a { chainloader /a.efi }").message,
            "search --label requires a label"
        );
        assert_eq!(
            parse_err("search --file /a --label b\nmenuentry a { chainloader /a.efi }").message,
            "search accepts only one identifier option"
        );
        assert_eq!(
            parse_err("search --set=bad-name --label b\nmenuentry a { chainloader /a.efi }")
                .message,
            "invalid search variable name"
        );
        assert_eq!(
            parse_err("search --bogus\nmenuentry a { chainloader /a.efi }").message,
            "unsupported search option or argument"
        );
        let ok = parse_ok("search -n -s -f /EFI/x.efi --hint hd0,gpt1 --hint-efi=hd0\nmenuentry a { chainloader /a.efi }");
        assert_eq!(
            ok.config.entries[0].volume,
            VolumeSelector::File("\\EFI\\x.efi".to_string())
        );
        let uuid = parse_ok("search -u abcd-ef01 --set=other\nmenuentry a { chainloader /a.efi }");
        assert_eq!(uuid.config.entries[0].volume, VolumeSelector::Own);
    }

    #[test]
    fn title_validation() {
        let long = "t".repeat(MAX_TITLE_CHARS + 1);
        assert_eq!(
            parse_err(&format!("menuentry '{long}' {{ chainloader /a.efi }}")).message,
            "menu title exceeds 256 characters"
        );
        let exact = "t".repeat(MAX_TITLE_CHARS);
        assert_eq!(
            parse_ok(&format!("menuentry '{exact}' {{ chainloader /a.efi }}"))
                .config
                .entries[0]
                .title,
            exact
        );
        assert_eq!(
            parse_ok("menuentry '  padded  ' { chainloader /a.efi }")
                .config
                .entries[0]
                .title,
            "padded"
        );
        // A quoted newline is a control character in the expanded title.
        assert_eq!(
            parse_err("menuentry 'a\nb' { chainloader /a.efi }").message,
            "control character in configuration"
        );
    }

    #[test]
    fn word_and_option_limits() {
        let long_word = format!(
            "set big='{}'\nmenuentry a {{ chainloader /a.efi }}",
            "x".repeat(MAX_WORD_BYTES + 1)
        );
        assert_eq!(parse_err(&long_word).message, "word exceeds 4096 bytes");
        let args = (0..700).map(|_| "argument12").collect::<Vec<_>>().join(" ");
        let options = format!("menuentry a {{ linux /k {args} }}");
        assert_eq!(
            parse_err(&options).message,
            "menuentry options exceed 4096 bytes"
        );
    }

    #[test]
    fn variable_limit_is_enforced() {
        let mut text = String::new();
        for i in 0..MAX_VARIABLES {
            text.push_str(&format!("set v{i}=1\n"));
        }
        text.push_str("menuentry a { chainloader /a.efi }\n");
        // The predefined menuentry_id_option counts toward the limit.
        assert_eq!(parse_err(&text).message, "too many variables");
    }

    #[test]
    fn entry_scope_does_not_leak_but_top_level_sets_persist() {
        let config = parse_ok(
            "set base=/top\n\
             menuentry a { set base=/inner; chainloader ${base}/a.efi }\n\
             menuentry b { chainloader ${base}/b.efi }\n",
        );
        assert_eq!(config.config.entries[0].path, "\\inner\\a.efi");
        assert_eq!(config.config.entries[1].path, "\\top\\b.efi");
    }

    #[test]
    fn id_generation_and_validation() {
        let config = parse_ok(
            "menuentry a --id good-1 { chainloader /a.efi }\n\
             menuentry b --id good-1 { chainloader /b.efi }\n\
             menuentry c --id 'bad id' { chainloader /c.efi }\n\
             menuentry d { chainloader /d.efi }\n",
        );
        let ids: Vec<&str> = config
            .config
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["good-1", "grub-2", "grub-3", "grub-4"]);
        assert_eq!(config.warnings.len(), 2);
        assert!(config.warnings.iter().all(|w| w.message.contains("--id")));
    }

    #[test]
    fn generated_id_skips_taken_values() {
        let config = parse_ok(
            "menuentry a --id grub-2 { chainloader /a.efi }\nmenuentry b { chainloader /b.efi }\n",
        );
        let ids: Vec<&str> = config
            .config
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["grub-2", "grub-3"]);
    }

    #[test]
    fn unset_and_export_are_accepted() {
        let config = parse_ok(
            "set root=plain\nunset root\nexport gfxpayload\n\
             menuentry a { chainloader /a.efi }\n",
        );
        assert_eq!(config.config.entries[0].volume, VolumeSelector::Own);
        assert_eq!(
            parse_err("unset\nmenuentry a { chainloader /a.efi }").message,
            "unset requires exactly one variable name"
        );
        assert_eq!(
            parse_err("export a b\nmenuentry a { chainloader /a.efi }").message,
            "export requires exactly one variable name"
        );
    }

    #[test]
    fn set_without_equals_and_invalid_names_fail() {
        assert_eq!(
            parse_err("set timeout\nmenuentry a { chainloader /a.efi }").message,
            "set requires NAME=VALUE"
        );
        assert_eq!(
            parse_err("set a-b=1\nmenuentry a { chainloader /a.efi }").message,
            "invalid variable name"
        );
        assert_eq!(
            parse_err("set a b\nmenuentry a { chainloader /a.efi }").message,
            "set requires exactly one NAME=VALUE argument"
        );
    }

    #[test]
    fn no_effect_commands_warn_with_line() {
        let config =
            parse_ok("insmod ext2\necho hi\nmenuentry a { sleep 1; chainloader /a.efi }\n");
        let lines: Vec<usize> = config.warnings.iter().map(|w| w.line).collect();
        assert_eq!(lines, vec![1, 2, 3]);
    }

    #[test]
    fn grub_config_path_constant_is_stable() {
        assert_eq!(GRUB_CONFIG_PATH, "\\EFI\\BOOTAPP\\grub.cfg");
    }

    /// Deterministic mutation sweep: no input may panic, and every rejection
    /// carries a sane line number.
    #[test]
    fn fuzz_mutations_never_panic() {
        let samples = [DEBIAN, WINDOWS, SUBMENU];
        let alphabet: &[u8] = b"{}'\"\\$#;\n =()/,>-x";
        let mut checked = 0usize;
        for sample in samples {
            let bytes = sample.as_bytes();
            for cut in 0..=bytes.len() {
                let text = String::from_utf8(bytes[..cut].to_vec()).unwrap();
                check_outcome(&text);
                checked += 1;
            }
            for index in 0..bytes.len() {
                for &replacement in alphabet {
                    let mut mutated = bytes.to_vec();
                    mutated[index] = replacement;
                    let text = String::from_utf8(mutated).unwrap();
                    check_outcome(&text);
                    checked += 1;
                }
                let mut removed = bytes.to_vec();
                removed.remove(index);
                check_outcome(&String::from_utf8(removed).unwrap());
                checked += 1;
            }
        }
        assert!(checked > 1000);
    }

    fn check_outcome(text: &str) {
        match parse(text) {
            Ok(config) => {
                assert!(!config.config.entries.is_empty());
                assert!(config
                    .config
                    .entries
                    .iter()
                    .any(|e| e.id == config.config.default));
                assert!(config.config.entries.len() <= MAX_ENTRIES);
            }
            Err(error) => assert!(error.line <= text.lines().count() + 1),
        }
    }

    #[test]
    fn arbitrary_bytes_are_handled_without_panic() {
        let mut seed: u32 = 0x1234_5678;
        for _ in 0..200 {
            let mut text = String::new();
            for _ in 0..200 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let byte = (seed >> 16) as u8;
                let c = match byte % 8 {
                    0 => '{',
                    1 => '}',
                    2 => '\'',
                    3 => '"',
                    4 => '$',
                    5 => '\n',
                    6 => ' ',
                    _ => char::from(b'a' + byte % 26),
                };
                text.push(c);
            }
            check_outcome(&text);
        }
    }
}
