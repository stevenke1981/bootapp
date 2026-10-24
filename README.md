# BootApp v0.2

Rust 開機管理器：Legacy BIOS + x86／x64／ARM64 UEFI。
提供選單、倒數、跨磁碟區自動偵測與既有作業系統載入器的 chainload。
不宣稱完整 GRUB 2 指令相容或所有硬體均已認證。

## 功能

- BIOS：獨立組語 bootstrap 載入 Rust 管理器，經 BIOS EDD 掃描其他磁碟，
  顯示 GRUB／Windows NTFS boot loader 證據，選擇後以正確 DL 進入該磁碟 MBR。
- UEFI：列舉韌體可讀的各個磁碟區，辨識 Windows Boot Manager、Linux
  shim／GRUB／systemd-boot／UKI、macOS boot.efi 與通用 EFI fallback。
- macOS：CoreServices、Apple EFI、Preboot UUID 與 Recovery 路徑。
  讀取能力取決於韌體提供的檔案系統；沒有內建 APFS／HFS+ 驅動。
- 自動候選必須符合 PE 格式、EFI application subsystem 與目前韌體架構。
  路徑與 PE 檢查是載入器線索，實際 OS 開機驗收另行記錄。
- 每個 UEFI 選項保留所屬磁碟區，避免不同磁碟的相同路徑啟動錯誤目標。
- Windows x64 設定工具：validate、preview、scan、stage；掃描唯讀並排除連結。

## 建置

```powershell
rustup target add i686-unknown-uefi x86_64-unknown-uefi aarch64-unknown-uefi
.\scripts\build.ps1

rustup target add i686-unknown-linux-gnu
rustup component add llvm-tools-preview
python scripts/build-bios.py --nasm C:/path/to/nasm.exe --output out/bios
```

BIOS 另需 NASM；建置只產生專案映像檔，拒絕覆寫既有輸出。
一般 Cargo workspace 排除 freestanding BIOS crate；其記憶體與 ABI 契約見
[BIOS 文件](crates/bootapp-bios/README.md)。

## UEFI 使用

```powershell
target/release/bootapp.exe scan x64 D:/mounted-esp-1 D:/mounted-esp-2
target/release/bootapp.exe validate examples/bootapp.cfg
target/release/bootapp.exe stage examples/bootapp.cfg x64 target/x86_64-unknown-uefi/release/bootapp-efi.efi new-staging-directory
```

自動模式只需將對應管理器放到 FAT 媒體的 EFI/BOOT：

| 韌體架構 | 檔名 |
|---|---|
| IA32 / x86 UEFI | BOOTIA32.EFI |
| X64 UEFI | BOOTX64.EFI |
| AArch64 UEFI | BOOTAA64.EFI |

沒有 EFI/BOOTAPP/bootapp.cfg 時自動掃描；有設定檔則合併自動結果並保留
明確指定的預設選項。設定路徑仍指向管理器所在磁碟區；錯誤設定不會靜默忽略。
↑／↓ 選擇、Enter 啟動、任意鍵停止倒數；UEFI 另有 R 重開機、Q 返回韌體。

## 驗收與限制

實際結果、映像來源與重現方式見 [驗收摘要](docs/VALIDATION.md)。
EFI fixture、已安裝作業系統、實體硬體與 Secure Boot 分開計算。

BIOS 只啟動既有磁碟 MBR，由它處理 active partition 與後續 loader。
沒有原生 bzImage／Multiboot 載入，不會把 EFI-only GPT Windows 當成 BIOS 系統。
BIOS 掃描最多八個 EDD 硬碟，需要韌體提供磁碟與鍵盤服務。
CPU、韌體與 loader 的架構都必須匹配。

沒有實體磁碟安裝器，不會寫主機 MBR／ESP、修改 BootOrder、安全金鑰或主機網路。
未簽章開發版本需在允許載入的韌體政策下使用。
實體硬體、Secure Boot 與真正 macOS 開機仍另列 NOT_RUN。

## 規格與授權

[架構](docs/ARCHITECTURE.md)、[設定契約](SPEC.md)、[偵測契約](docs/DETECTION.md)。
程式採 MIT OR Apache-2.0 雙授權，見 LICENSE-MIT 與 LICENSE-APACHE。

參考：[Rust UEFI targets](https://doc.rust-lang.org/rustc/platform-support/unknown-uefi.html)、
[uefi-rs](https://github.com/rust-osdev/uefi-rs)、
[UEFI boot manager](https://uefi.org/specs/UEFI/2.10/03_Boot_Manager.html)、
[SeaBIOS timer](https://github.com/coreboot/seabios/blob/master/src/clock.c)。
