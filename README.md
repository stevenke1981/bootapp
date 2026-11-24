# BootApp v0.3

Rust 開機管理器：Legacy BIOS + x86／x64／ARM64 UEFI。
提供選單、倒數、跨磁碟區自動偵測、GRUB `grub.cfg` 子集與既有作業系統載入器的 chainload。
這不是 GRUB 2 的相容實作：不執行 GRUB script，只接受文件列出的指令子集。
不宣稱所有硬體均已認證。

## 功能

- BIOS：獨立組語 bootstrap 載入 Rust 管理器，經 BIOS EDD 掃描其他磁碟，
  顯示 GRUB／Windows NTFS boot loader 證據，選擇後以正確 DL 進入該磁碟 MBR。
  已安裝到本機磁碟時，另列出該磁碟的主分割區 VBR 與「已儲存的原始 MBR」（後者以目前的分割表開機；
  空白的已儲存 MBR 不列出）。
  倒數即時更新（5、4、3、2、1 秒）；任一鍵暫停，Enter 立即啟動。
- UEFI：列舉韌體可讀的各個磁碟區，辨識 Windows Boot Manager、Linux
  shim／GRUB／systemd-boot／UKI、macOS boot.efi 與通用 EFI fallback。
- 設定優先序：`\EFI\BOOTAPP\bootapp.cfg` → `\EFI\BOOTAPP\grub.cfg` → 僅自動偵測。
  自動偵測結果一律附加在明確設定的項目之後；設定錯誤會顯示在畫面上，不會被靜默忽略。
- GRUB 相容子集（[規格](docs/GRUB.md)）：`menuentry`／`submenu`、`chainloader`、
  `linux`／`linuxefi` 加 `initrd`、`search --file|--fs-uuid|--label`、`set timeout|default`、
  `timeout=-1`。`set root=$var` 沿用搜尋結果；未經搜尋的 root 文字在使用時報錯。
  submenu 內的 `timeout`／`default` 會被忽略並警告；核心參數含空白時以 GRUB 風格加引號；
  後面的 `initrd` 取代前面的清單；grub-mkconfig 產生的 `--id` 可作為 `set default=` 目標。
  不支援 script、條件判斷、迴圈、函式與載入其他設定檔；
  不支援 ext4／btrfs 等驅動，該磁碟區必須能被韌體讀取。
  `grub-check` 可在主機上預先檢查 `grub.cfg`。
- 安裝器（[安裝指南](docs/INSTALL.md)）：`bootapp install uefi` 寫入已掛載的 ESP 目錄；
  `bootapp install bios` 寫入 MBR 磁碟映像或 `\\.\PhysicalDriveN`。
  預設只顯示計畫（dry-run），需 `--apply`；BIOS 寫入前建立新的備份檔（`BOOTAPP_BIOS_BACKUP 1`，
  125,440 bytes，含原始與安裝後的 LBA0..=121）。
  UEFI 安裝留下 manifest，`uninstall uefi` 只還原清單內記錄的變更；遭外部修改的檔案會保留並回報，
  此時以非零狀態結束。`uninstall bios` 若發現非 BootApp 也非原始的磁區內容會拒絕，`--force` 為危險選項。
- BootOrder：UEFI 選單中按 `F` 可將 BootApp 放到韌體開機順序第一位，
  且只寫入 BootApp 自己的 `Boot####` 與 `BootOrder`。
  Windows 主機可用 `bootorder list／set／next`（預設 dry-run，需系統管理員權限）。
- Secure Boot：使用自有金鑰簽署與啟用流程見 [Secure Boot 指南](docs/SECURE-BOOT.md)；
  本專案不自動更換韌體金鑰。
- 每個 UEFI 選項保留所屬磁碟區，避免不同磁碟的相同路徑啟動錯誤目標。
- Windows x64 設定工具：validate、preview、scan、stage；掃描唯讀並排除連結。
- macOS：CoreServices、Apple EFI、Preboot UUID 與 Recovery 路徑。
  讀取能力取決於韌體提供的檔案系統；沒有內建 APFS／HFS+ 驅動。
- 自動候選必須符合 PE 格式、EFI application subsystem 與目前韌體架構。
  路徑與 PE 檢查是載入器線索，實際 OS 開機驗收另行記錄。

## 建置

```powershell
rustup target add i686-unknown-uefi x86_64-unknown-uefi aarch64-unknown-uefi
.\scripts\build.ps1

cargo build --release -p bootapp-cli

rustup target add i686-unknown-linux-gnu
rustup component add llvm-tools-preview
python scripts/build-bios.py --nasm C:/path/to/nasm.exe --output out/bios
```

BIOS 另需 NASM；建置只產生專案映像檔，拒絕覆寫既有輸出。
一般 Cargo workspace 排除 freestanding BIOS crate；其記憶體與 ABI 契約見
[BIOS 文件](crates/bootapp-bios/README.md)。
CI 設定見 [.github/workflows/ci.yml](.github/workflows/ci.yml)。

## UEFI 使用

```powershell
target/release/bootapp.exe scan x64 D:/mounted-esp-1 D:/mounted-esp-2
target/release/bootapp.exe validate examples/bootapp.cfg
target/release/bootapp.exe grub-check <path>\grub.cfg x64
target/release/bootapp.exe stage examples/bootapp.cfg x64 target/x86_64-unknown-uefi/release/bootapp-efi.efi new-staging-directory
```

自動模式只需將對應管理器放到 FAT 媒體的 EFI/BOOT：

| 韌體架構 | 檔名 |
|---|---|
| IA32 / x86 UEFI | BOOTIA32.EFI |
| X64 UEFI | BOOTX64.EFI |
| AArch64 UEFI | BOOTAA64.EFI |

設定檔路徑仍指向管理器所在磁碟區；錯誤設定不會靜默忽略。
↑／↓ 選擇、Enter 啟動、任意鍵停止倒數；UEFI 另有 F 將 BootApp 設為 BootOrder 第一位、
R 重開機、Q 返回韌體。

## 安裝與 BootOrder（系統管理員）

安裝器預設只列出計畫，不改變任何檔案或磁區。確認計畫後加上 `--apply`。
完整步驟、回復方式與 manifest 格式見 [docs/INSTALL.md](docs/INSTALL.md)。

```powershell
# UEFI：先以系統管理員身分掛載 ESP，再執行計畫（dry-run）
mountvol S: /s
target\release\bootapp.exe install uefi --esp S:\ --arch x64 --manager target\x86_64-unknown-uefi\release\bootapp-efi.efi
target\release\bootapp.exe uninstall uefi --esp S:\

# BIOS：只接受 MBR 磁碟映像或 \\.\PhysicalDriveN（後者需系統管理員，即使 dry-run）；備份檔必須是新檔
target\release\bootapp.exe install bios --target disk.img --bios-image out\bios\bootapp-bios.img --backup disk.backup.bin

# BootOrder（Windows 韌體變數）
target\release\bootapp.exe bootorder list
target\release\bootapp.exe bootorder set 0003,0001
```

## 驗收與限制

實際結果、映像來源與重現方式見 [驗收摘要](docs/VALIDATION.md)。
EFI fixture、已安裝作業系統、實體硬體與 Secure Boot 分開計算。
v0.2 的 BIOS QEMU 結果是以 Pentium III 等不含 SSE2 的 CPU 型號執行，對 SSE2 不具證據力；
v0.3 的 BIOS 測試改用 `-cpu pentium3,+sse2`，並新增不含 SSE2 的拒絕測試。

- BIOS 只做 chainload：其他磁碟的 MBR、本機磁碟主分割區的 VBR，或已儲存的原始 MBR，
  由它們處理後續 loader。沒有原生 bzImage／Multiboot 載入，不會把 EFI-only GPT Windows 當成 BIOS 系統。
  BIOS 掃描最多八個 EDD 硬碟，需要韌體提供磁碟與鍵盤服務。CPU 必須支援 SSE2：開機時以 CPUID 檢查 FXSR、SSE、SSE2，
  不符合則顯示 `BOOTAPP_BIOS_CPU_UNSUPPORTED` 並停止。韌體與 loader 的架構都必須匹配。
- GRUB 子集只解析文件列出的指令；未知或不支援的指令會以行號報錯，無作用的指令（例如 `insmod`）只發出警告。
  設定錯誤不會被靜默忽略。
- 安裝器與 BootOrder 寫入：預設 dry-run；實際寫入需要 `--apply`。映像檔與 ESP 目錄不需系統管理員權限；
  `--register`、`bootorder`（含 `list`）、已註冊安裝的 `uninstall uefi`，以及 `\\.\PhysicalDriveN`（含 dry-run）需要系統管理員權限。
  目標若是執行中的 Windows 磁碟或韌體開機分割區，還需 `--allow-system-disk`。
- 安裝器與 BootOrder 寫入只在磁碟映像、ESP 目錄與 QEMU 私有 NVRAM 上驗證；
  本專案在開發環境中未對實體硬碟或主機韌體執行寫入。
- 未簽章開發版本需在允許載入的韌體政策下使用。
- 尚未執行（NOT_RUN）：實體硬體、Secure Boot 實機開機、真正 macOS 開機、
  真正 Linux kernel EFI-stub 開機、WHPX／KVM 加速。
  GitHub Actions 託管 CI（Linux 與 Windows，含 QEMU 測試）已全數通過。

## 規格與授權

[架構](docs/ARCHITECTURE.md)、[設定契約](SPEC.md)、[偵測契約](docs/DETECTION.md)、
[GRUB 子集](docs/GRUB.md)、[安裝指南](docs/INSTALL.md)、[Secure Boot](docs/SECURE-BOOT.md)、
[變更紀錄](CHANGELOG.md)。
程式採 MIT OR Apache-2.0 雙授權，見 LICENSE-MIT 與 LICENSE-APACHE。

參考：[Rust UEFI targets](https://doc.rust-lang.org/rustc/platform-support/unknown-uefi.html)、
[uefi-rs](https://github.com/rust-osdev/uefi-rs)、
[UEFI boot manager](https://uefi.org/specs/UEFI/2.10/03_Boot_Manager.html)、
[SeaBIOS timer](https://github.com/coreboot/seabios/blob/master/src/clock.c)。
