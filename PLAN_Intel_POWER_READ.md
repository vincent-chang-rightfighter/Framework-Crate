# Role & Task

實作一個 Rust 利用 **PawnIO 驅動程式** 安全地「唯讀（Read-Only）」讀取當前 Intel Core Ultra 5 125H（Framework 13 筆電）的 PL1 和 PL2 的靜態與動態功耗限制，並將其轉換成人類可讀的瓦數（Watts）顯示出來。

這是我們實作 "Sync MMIO" 強制降功耗功能的第一步，必須先確保讀取路徑 100% 正確。

# Dependencies Constraints

- **關鍵限制**：必須完全使用 **`windows-sys = { version = "0.61.2", features = ["Win32_System_IO", "Win32_Storage_FileSystem", "Win32_Foundation"] }`** 進行底層 Windows API FFI 調用。
- 禁止使用 `windows` crate。所有的 Windows 函數（如 `CreateFileA`, `DeviceIoControl`, `CloseHandle`）和常數（如 `GENERIC_READ`, `OPEN_EXISTING`）都必須來自 `windows-sys`。
- `HANDLE` 在此版本中是 `isize` 或 `*mut c_void`（請依據 windows-sys 0.61.2 標準嚴格編寫），所有調用必須封裝在正確的 `unsafe` 區塊中。

# Environment Context

- **驅動程式來源**：本系統中的 PawnIO 驅動程式是透過 Windows 封裝管理器安裝的官方最新版本，安裝命令為 `winget install -e --id namazso.PawnIO`。
- **裝置路徑**：驅動安裝並啟動後，在 Windows 符號連結命名空間中註冊為 `\\.\PawnIO`。

# Technical Specifications & Addresses

請在 Rust 中實作以下硬體定位與解碼邏輯：

1. **取得 MCHBAR 基底地址 (PCI 讀取)**
   - 透過呼叫 PawnIO 提供的 PCI 讀取 IOCTL，讀取 PCI 配置空間 **`Bus 0, Device 0, Function 0`** 的 **Offset `0x48`**（64位元暫存器）。
   - 遮蔽清除低位元旗標（將低 12 位元清零，即 `& !0xFFF`），取得當前的 `MCHBAR_BASE` 實體記憶體基底地址。

2. **定位讀取目標**
   - **MSR 靜態牆地址**：`0x610` (`MSR_TURBO_POWER_LIMIT`)
   - **MMIO 動態牆地址**：`MCHBAR_BASE + 0x59A0` (`PACKAGE_POWER_LIMIT_MMIO`)
   - **MSR 功率單位地址**：`0x606` (`MSR_RAPL_POWER_UNIT`)。我們需要讀取 `0x606` 的 **Bits 0-3**，將其代入公式 `1.0 / (1 << bits)` 來得知 1 個單位代表多少瓦（通常 Core Ultra 數值為 3，代表 1/8 = 0.125 瓦）。

3. **資料解碼公式 (64-bit Payload Decode)**
   不論是 MSR 0x610 還是 MMIO 記憶體，讀出來的 64 位元（u64）格式均相同：
   - **PL1 原始值**：讀取 `Bits 0-14`，並檢查 `Bit 15` 是否為 Enable，`Bit 16` 是否為 Clamp。
   - **PL2 原始值**：讀取 `Bits 32-46`，並檢查 `Bit 47` 是否為 Enable，`Bit 48` 是否為 Clamp。
   - **最終瓦數**：`實際瓦數 = 原始值 * 功率單位 (如 0.125)`。

# Code Architecture Requirements

請為我生成包含以下部分的 Rust 程式碼：

1. 使用 `windows-sys` 的 `CreateFileA` 初始化並獲取 PawnIO 驅動程式控制代碼（指向 `\\\\.\\PawnIO`）。請注意 C 字串結尾的 `\0` 處理。
2. 根據 `namazso/PawnIO` 的官方內核直譯器或 RPC 通訊規格，在 Rust 中手動定義所需的 IOCTL 控制碼常數（如利用 `CTL_CODE` 巨集公式計算）與請求結構體。
3. 實作 `fn get_mchbar_base(pawnio_handle: windows_sys::Win32::Foundation::HANDLE) -> u64`。
4. 實作解碼函數 `fn decode_power_limit(raw_val: u64, unit: f64) -> (f64, bool, bool)`，回傳 `(瓦數, Enabled, Clamped)`。
5. 在函數中，依序讀取並在終端機（Console）精美列印出：
   - 目前系統的 Power Unit 單位（瓦/單位）
   - MSR 0x610 讀出的：PL1/PL2 瓦數、Enable 狀態、Clamp 狀態
   - MMIO (Dynamic) 讀出的：PL1/PL2 瓦數、Enable 狀態、Clamp 狀態
