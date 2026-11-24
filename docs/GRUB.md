# GRUB 2 `grub.cfg` compatibility subset

BootApp can read a small, documented subset of a GRUB 2 `grub.cfg` and turn it into
the same menu entries as `bootapp.cfg`. Nothing is executed. Every construct is either
mapped to an entry, accepted with a warning (no effect), or rejected with a line number.
Nothing is silently ignored.

Implementation: `crates/bootapp-core/src/grub.rs` (`grub::parse`).

## Where the file is read

| Order | File on the manager's own volume | Used when |
|---|---|---|
| 1 | `\EFI\BOOTAPP\bootapp.cfg` | present (highest precedence) |
| 2 | `\EFI\BOOTAPP\grub.cfg` | `bootapp.cfg` absent |
| 3 | none (auto-discovery only) | both absent |

Auto-discovered loaders are appended after the configured entries in cases 1 and 2.

## Lexical rules

- Commands end at a newline or `;`. Words are separated by spaces or tabs.
- `#` starts a comment only at the start of a word. `a#b` is one word.
- `'...'` is literal and may span lines.
- `"..."` may span lines. Inside it, `\"`, `\\` and `\$` are escapes, other backslashes are
  literal, and `$var` / `${var}` are expanded.
- Unquoted `\x` yields `x`. A backslash before a newline is an error.
- `$var` and `${var}` expand in unquoted words too. Names are `[A-Za-z0-9_]+`. Unset
  variables (including positional `$1`) expand to the empty string.
- A standalone `{` or `}` word is a block token. `{` attached to other text is part of
  that word.
- CRLF line endings are accepted. Other control characters are errors. A TAB is allowed
  as whitespace between words and inside comments, but a TAB inside a quoted or expanded
  word is an error.
- Errors name the source line. Multi-line quotes report the line where the quote opened.

## Limits

| Item | Limit |
|---|---|
| File size | 64 KiB (`MAX_CONFIG_BYTES`) |
| Entries (after submenu flattening) | 128 (`MAX_ENTRIES`) |
| Submenu nesting | 8 levels (`MAX_SUBMENU_DEPTH`) |
| One word, after expansion | 4096 bytes |
| Entry options | 4096 bytes |
| Variables | 256 (including the predefined one) |
| Entry title | 256 characters, after trimming |
| `timeout` | `-1` (wait forever) or 0 to 3600 seconds |

## Supported commands

Top level and inside `submenu`:

- `set NAME=VALUE`. `NAME` is `[A-Za-z0-9_]+`. `set NAME="..."` works. Setting `timeout`
  and `default` at top level has the meaning given below (inside a submenu they are
  ignored with a warning, see "Submenu scope"). Any other variable is stored. When the
  value is exactly one variable reference (`set root=$dev` or `set root=${dev}`), the
  search selector of that variable is copied (see "Volume selection").
- `unset NAME`, `export NAME` (variables are already global, so `export` only validates).
- `search [-n|--no-floppy] [--set[=VAR]] (-f|--file|-u|--fs-uuid|-l|--label) [--hint* X] KEY`.
  `VAR` defaults to `root`. The key is the trailing argument. `--hint*` options and
  `--no-floppy` are ignored.
- `menuentry TITLE [--class C] [--id ID | $menuentry_id_option ID] [--unrestricted]
  [--users U] [--hotkey K] { ... }`
- `submenu TITLE [same options] { ... }`. Children are flattened, and their titles are
  `TITLE > child`.

Inside `menuentry`:

- `set`, `unset`, `export`, `search` (as above). Variables set inside an entry are
  discarded when the entry ends.
- `chainloader PATH`
- `linux PATH args...` and `linuxefi PATH args...`
- `initrd PATH [PATH...]` and `initrdefi PATH [PATH...]`. These need a preceding `linux`.
  Each `initrd` command replaces the initrd list of the previous `initrd` command, as GRUB
  does, so only the last one counts.
- `boot` (no effect, silent)

Submenu scope: a `submenu` body runs only when the user enters it, so it is scoped like
an entry. Variables set inside it (including `search --set`) are discarded when the
submenu ends. `set timeout` and `set default` inside a submenu are ignored with a warning
and do not change the file's timeout or default.

Accepted with a warning and no effect (they run in GRUB but do nothing in BootApp):
`insmod`, `load_video`, `terminal_input`, `terminal_output`, `terminal`, `font`,
`loadfont`, `gfxmode`, `echo`, `background_image`, `play`, `sleep`, `serial`.

Stored without a warning: `set` of display or language variables such as `gfxpayload`,
`timeout_style`, `pager`, `lang`. They have no effect on BootApp.

## Rejected (error with line number)

- Conditionals and loops: `if`, `then`, `else`, `elif`, `fi`, `for`, `while`.
- `function`, `source`, `.`, `configfile`, `normal`.
- Unknown commands.
- `menuentry` or `submenu` without `{`, and a `{` that does not follow a menu command.
- A missing `}` at end of file, and an unexpected `}`.
- `menuentry` nested inside `menuentry`.
- A menu command inside an entry, or a loader command outside an entry.
- An entry with zero loader commands, or with both `chainloader` and `linux`.
- `initrd` without a preceding `linux`, or `initrd` with a space in its path.
- `chainloader +N` (BIOS block lists).
- GRUB device names such as `(hd0,gpt1)`, `(hd0)`, `(cd)`. Use `search` instead.
- Relative paths, `..` components, and paths failing `validate_volume_path`.
- `search` with no identifier, with two identifiers, with a malformed `--fs-uuid`
  (must be `XXXX-XXXX`), or with a `--file` path that is not absolute.
- `timeout` outside -1 or 0 to 3600.
- A `default` that names no entry.
- Title or option limits exceeded, or an empty title.

## Mapping to BootApp entries

| GRUB | BootApp `Entry` |
|---|---|
| `menuentry TITLE` | `title` = TITLE (submenu prefix applied). Trimmed. |
| `--id ID` or `$menuentry_id_option ID` | `id` = ID when valid (`[A-Za-z0-9_-]`, at most 64 characters) and unused. Otherwise a warning and `grub-<n>` is generated, where `n` is the 1-based position in the flattened list. The raw ID text is kept for `set default=` (see "Default selection"). |
| `chainloader P` | `path` = P, `options` empty |
| `linux P a b` with `initrd A B` | `path` = P, `options` = `a b initrd=\A initrd=\B` (kernel arguments quoted as described below) |
| `search ... --set=root` | the entry volume: `--file` gives `File`, `--fs-uuid` gives `FsUuid`, `--label` gives `Label` |
| no `search`, or `root` empty | `Own` (the manager's own volume) |

Kernel arguments: each `linux` argument that is not empty after expansion is emitted
as follows, which matches GRUB's loader command line. An argument that contains a space,
a TAB, or `"` is wrapped in double quotes, and inside the quotes every `\` and every `"`
is preceded by a backslash. Other arguments are emitted unchanged. The arguments are
then joined with single spaces. Example:
`linux /vmlinuz 'acpi_osi=Windows 2020' quiet` gives
`"acpi_osi=Windows 2020" quiet`. An argument `a"b` gives `"a\"b"`, and `c:\x y` gives
`"c:\\x y"`.

Path rules: `/` becomes `\`. The path must be absolute and pass `validate_volume_path`.
`($root)` and `($VAR)` select the volume of a search result. A bare device name is an
error. All paths of one entry must be on the same volume (for example, `linux` and
`initrd` after a second `search` that changes `root` is an error), because Linux resolves
`initrd=` on the kernel's volume.

Volume selection for `root`:

- `root` unset or set to the empty string: `Own`.
- `root` holds a search result (`search --set`, or `set root=$VAR` where `$VAR` holds a
  search result, including `set root=${VAR}`): that volume. `set root=$VAR` copies the
  selector, so `search --fs-uuid --set=dev ABCD-1234` followed by `set root=$dev` selects
  `FsUuid("ABCD-1234")`.
- `root` holds any other non-empty literal (for example `set root=hd0,gpt1`,
  `set root=plain`, or `set root=$dev/x`): an error when an entry uses it, with the
  message `root is not a search result; use search`. Literal values are never mapped to
  a volume.
- `($root)` and `($VAR)` in a path use the same rules for `root` and for `VAR`.

Setting `root` literally after a search replaces the search result, so a later entry
that uses `root` is an error until another `search` sets it again.

## Default selection

`set default=X` is resolved after the whole file is read:

- `N` (digits only): the N-th top-level item, counting from 0. A submenu counts as one item.
- `N>M>...`: the path into submenus. The path must end on an entry.
- Otherwise, the first entry whose BootApp id equals X, then the first entry whose raw
  `--id` text equals X, then the first entry whose flattened title equals X exactly. The
  raw `--id` text is kept only when it is at most 256 characters. This lets a
  grub-mkconfig id such as `gnulinux-6.1.0-13-amd64-advanced-0123abcd` (which contains
  dots, so BootApp generates `grub-<n>` for it) name its entry.
- `saved`, an empty value, or an empty expansion (such as an unset variable): the first
  entry, with a warning.
- Anything else: error (`default references an unknown entry`).

Without `set default`, the first entry is used. `timeout` defaults to 5 seconds when not set.

## Linux EFI stub requirement

BootApp boots a Linux kernel only through the EFI stub. This means:

- The kernel must be built with `CONFIG_EFI_STUB=y` (x86_64 and aarch64). Distribution
  kernels normally are. Kernels without the stub cannot be started by a `linux` entry.
- The kernel and every initrd must be on the same FAT volume that UEFI firmware can read.
  Firmware reads only FAT (and, with a vendor driver, other filesystems). A kernel on
  ext4 or btrfs cannot be read by standard UEFI, so such an entry will not boot. Put
  `vmlinuz` and `initrd` in the ESP or another FAT partition.
- The `root=` kernel argument refers to the root filesystem, which can be ext4, btrfs,
  and so on. Only the boot files need FAT.

## Examples

Debian-style entry on a FAT partition with a known volume ID:

```
set timeout=5
set default="0"
search --no-floppy --fs-uuid --set=root ABCD-1234
menuentry 'Debian GNU/Linux' --class debian $menuentry_id_option 'gnu-linux-simple' {
    linux /vmlinuz-6.12 root=UUID=1111-2222 ro quiet
    initrd /initrd.img-6.12
}
```

Maps to the entry: `id` = `gnu-linux-simple`, `path` = `\vmlinuz-6.12`,
`options` = `root=UUID=1111-2222 ro quiet initrd=\initrd.img-6.12`,
`volume` = `FsUuid("ABCD-1234")`.

Windows Boot Manager, found by file:

```
search --file --set=root /EFI/Microsoft/Boot/bootmgfw.efi
menuentry 'Windows Boot Manager' --id windows-boot {
    chainloader /EFI/Microsoft/Boot/bootmgfw.efi
}
```

Submenu with a default inside it:

```
set default="1>0"
menuentry 'First' { chainloader /A.efi }
submenu 'More' {
    menuentry 'Second' { chainloader /B.efi }
}
```

The default is `More > Second`.
