# Secure Boot and BootApp

Status (v0.3): **BootApp binaries are not signed.** The release EFI files
(`BOOTX64.EFI`, `BOOTIA32.EFI`, `BOOTAA64.EFI`) and the payload are unsigned. On a
machine with Secure Boot enabled, firmware refuses to start them. This guide explains the
options and the commands to try them. Nothing here has been verified on real hardware.

## How Secure Boot affects BootApp

- Firmware with Secure Boot checks every EFI image it loads (the boot manager itself and
  every image it starts through `LoadImage`/`StartImage`) against the `db` signature list
  and the `dbx` revocation list.
- BootApp chainloads other loaders (Windows `bootmgfw.efi`, GRUB, shim, and so on). Under
  Secure Boot each of those must also be trusted. A Microsoft-signed Windows loader
  works only if the Microsoft keys (KEK and db entries) remain enrolled.
- Replacing the platform key (PK) with your own usually removes the Microsoft keys
  unless you add them back. Doing that on a machine you need for Windows requires care.

## Options

### (a) Enroll your own PK / KEK / db keys

Use this when you want BootApp signed by a key you own and still want Secure Boot on.

- Risks: a wrong key set can leave a machine unable to boot Windows or Linux. Some
  firmware misbehaves when keys change. Signed option ROMs (for example some GPU or
  NIC firmware) may stop loading if the Microsoft db entries are removed. A
  firmware reset to factory keys may be the only recovery path on some boards.
- Keep the Microsoft KEK and db entries if you dual-boot Windows: add your KEK and db
  entries alongside them (append), do not replace them.
- Back up the firmware's current Secure Boot variables and your recovery key
  (BitLocker) before you start. Know how to reach firmware setup and how to restore
  factory keys on your board.
- Never commit `*.key` files. Keep the key directory outside the repository.
- `scripts/sign-efi.py keys` writes **unencrypted** test keys. Use them for testing;
  decide deliberately before using any key on real hardware.

### (b) shim + MOK

Use a distribution-signed shim as the first-stage loader, enroll your key with
MOK (Machine Owner Key), and let shim verify BootApp. This is the common Linux route.
BootApp v0.3 does not ship or configure shim. This is a manual, unverified option.

### (c) Disable Secure Boot

Turn Secure Boot off in firmware setup. This is the simplest and the only option that
needs no key work. BootApp v0.3 runs in this mode.

## Commands

### Linux (or WSL Ubuntu)

Install the tools once (Debian/Ubuntu package names):

```sh
sudo apt install sbsigntool openssl efitools
```

Build the EFI binary and create keys in a directory outside the repository:

```sh
cargo build --locked --release -p bootapp-efi --target x86_64-unknown-uefi
python3 scripts/sign-efi.py keys --out "$HOME/bootapp-sb-keys" --cn "BootApp Local"
python3 scripts/sign-efi.py sign \
  --key "$HOME/bootapp-sb-keys/db.key" --cert "$HOME/bootapp-sb-keys/db.crt" \
  --in target/x86_64-unknown-uefi/release/bootapp-efi.efi \
  --out "$HOME/BOOTX64.EFI.signed"
python3 scripts/sign-efi.py verify --cert "$HOME/bootapp-sb-keys/db.crt" --in "$HOME/BOOTX64.EFI.signed"
sbverify --cert "$HOME/bootapp-sb-keys/db.crt" "$HOME/BOOTX64.EFI.signed"
```

The `keys` directory contains `PK`, `KEK`, `db` as `.key`, `.crt`, `.esl`, `.auth`.
Enrollment is a firmware-level change. The `.auth` files are the input for
`efi-updatevar` (for example `efi-updatevar -a -f db.auth db` appends to db instead of
replacing it). Check `efi-updatevar --help` on your system before using it; these
commands are not verified here.

### Windows

`sign-efi.py` needs Linux tools. On Windows, run it inside WSL:

```powershell
wsl --install -d Ubuntu        # once, from an administrator shell
wsl -- sudo apt install sbsigntool openssl efitools
wsl -- python3 /mnt/d/bootapp/scripts/sign-efi.py keys --out "$HOME/bootapp-sb-keys" --cn "BootApp Local"
```

Keep the key directory inside the Linux home directory (`~`), not under `/mnt/d`, so
private keys are not written to the Windows drive. Enrollment must be done from the
firmware setup screen or from a Linux environment; this project does not automate it.

Without WSL or MSYS2, `sign-efi.py` prints the missing tools and exits with status 3.

## Verification status

| Item | Status |
|---|---|
| `sign-efi.py` argument parsing and refusal paths (overwrite, in-place sign, existing key directory) | Checked with stubbed tools on Windows. Real tool calls NOT_RUN on Windows. |
| CI job `secure-boot-sign` (keys, sign, `sbverify`) | Defined in `.github/workflows/ci.yml`. Passes only when a CI run is green. NOT_RUN in this environment (no push). |
| BootApp booting under Secure Boot in QEMU (OVMF secure-code image) | NOT_RUN |
| Enrollment and boot on real hardware | NOT_RUN |

`sbverify` confirms the signature chains to the certificate you give it. It does not
prove that a given firmware will accept the image; that depends on the enrolled PK, KEK,
db and dbx on that machine.
