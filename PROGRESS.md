# Framework Crate 優化進度紀錄

最後更新：2026-08-13（第十五輪）

## 記錄格式

- 每一輪都依循相同格式：目標 → 結論 → 已採取動作 → 驗證結果。
- 內容以 bullet list 為主，避免長段落堆疊。
- Wording 以「行動導向、可驗證、可追蹤」為原則，保留關鍵修正與驗證證據。
- 若需要補充 backlog 或風險點，統一寫在「後續建議」或「待處理事項」區塊。

## 第十五輪：新功能需求规划 + 實作（2026-08-13）

### 目標

- 依優先順序規劃並實作 4 個新功能需求。
- 需求 A（高）：Expansion Card 分類修正 — USB-C 外接裝置接 Port 0 被誤判為 HDMI。
- 需求 B（中高）：平台功能篩選 — Framework Desktop 無 Battery/Fingerprint/Keyboard，需根據型號處理。
- 需求 C（中）：Multi-Fan 動態控制 — 多風扇機型需 per-fan 控制與 "Use Unified Duty" 選項。
- 需求 D（低）：About 頁 Debug 按鈕 — 收集純文字 debug report，作為未來偵錯基礎建設。

### 已採取動作

- **需求 A（高）：Expansion Card 分類修正** ✅
  - 調查 `framework_lib`：side expansion card slots 無 EC per-slot card ID API，僅能靠 PD 電氣特徵 heuristic 分類。
  - 修正 `classify_pd_port()`（`ec_wrapper.rs:147-160`）：當 `dp_alt_mode` 為 false 時，Source+PD 的 USB-C 裝置不再被誤判為 HDMI，改回傳 "USB-C Expansion Card"。
  - 新增 PD port 調試資訊顯示（`views.rs:950-958`）：每個 port 顯示 `[port] role=Source data=Dfp DP_ALT watts=5.0W`。
  - 增強 `pd_ports()` debug log（`ec_wrapper.rs:501`）：加入 `watts` 輸出。

- **需求 B（中高）：平台功能篩選** ✅
  - 新增 `PlatformFamily` enum + `detect_platform()`（`ec_wrapper.rs:56-87`）：Laptop12/13/16/Desktop/Unknown。
  - 新增 feature matrix：`has_battery()`、`has_fingerprint_led()`、`has_keyboard_backlight()`。
  - `SystemState` 新增 `platform: Arc<RwLock<Arc<PlatformFamily>>>`（`sub_state.rs:153`）。
  - `ViewSnapshot` 新增 `platform` 欄位（`views.rs:67`）。
  - 新增 `not_supported_section()` helper（`views.rs:69-85`）：淡紅色背景 + "Not Supported" 文字。
  - Battery 區塊（`views.rs:660`）、Keyboard Backlight（`views.rs:784`）、Fingerprint LED（`views.rs:791`）根據 platform 顯示 Not Supported。
  - 新增 `COLOR_NOT_SUPPORTED_BG` + `COLOR_NOT_SUPPORTED_TEXT`（`style.rs:8-9`）。

- **需求 C（中）：Multi-Fan 動態控制** ✅
  - `FanState` 新增 `fan_count`、`unified_duty`、`per_fan_duty`（`sub_state.rs:30-32`）。
  - 新增 `FanUnifiedDutyToggled`、`FanPerDutyChanged` messages（`app.rs:71-73`）。
  - `record_thermal_sample` 更新 `fan_count`（`background_task.rs:114`）。
  - Manual/Curve mode 支援 per-fan duty control（`background_task.rs:450-550`）。
  - Fan Control UI 新增 "Use Unified Duty" toggle + per-fan sliders（`views.rs:508-540`）。

- **需求 D（低）：About 頁 Debug 按鈕** ✅
  - 新增 `CollectDebugInfo`、`DebugInfoCollected` messages（`app.rs:96-97`）。
  - 實作 debug report 收集（`app.rs:848-900`）：platform、system info、fan state、thermal、battery、PD ports。
  - About 頁新增 "Collect Debug Info" 按鈕 + report 顯示（`views.rs:270-300`）。

- **Clippy 修正**：消除全部 12 個預存在的 `collapsible_if` + `manual_checked_ops` warnings。

### 驗證結果

- `cargo check`：0 errors
- `cargo clippy --all-targets`：0 warnings
- `cargo test --quiet`：104 passed, 0 failed

### 依賴影響

- 4 個需求均不引入新的 direct dependency。
- 調查結果：`framework_lib` 無 side expansion card slots 的 per-slot card ID API（僅 FW16 input deck 有 `CheckDeckState`，FW16 expansion bay 有 `ExpansionBayStatus`）。

## 第十一輪：Code Review backlog + 依賴精簡 / 資源效率 review（2026-08-12）

### 目標

- 再次檢查剩餘 backlog，確認是否仍有高價值清理點。
- 以現有依賴與資源效率目標為準，避免為了格式或命名風格做無意義改動。
- 將具體修正整合回目前進度紀錄，並保留已驗證的性能優化。

### 結論

- 直接依賴已經相當精簡；本輪未發現需要大幅度減少直接依賴的明顯問題。
- 真正值得處理的仍是 hot path：`App::update()` 的責任過重、config save 流程與 tray/quit lifecycle 的判斷混在一起。
- 本輪重點是「拆分責任邊界」，不重做格式化、也不改變既有資源效率設計。
- 依賴精簡的實務判斷已明確：直接依賴已接近合理下限，剩餘的重複主要來自上游 transitive dependency，而非專案內部亂添加 crate。

### 已採取動作

- `src/app.rs`：將 `App::update()` 的巨大分派邏輯拆成 `handle_config_message()`、`handle_tray_message()`、`handle_quit_message()`，讓 config / tray / quit 的處理有明確界線。
- 保持原有的 config 去抖、dirty-flag 快取、tray parking、視窗還原與 shutdown 流程不變，避免把性能修正與結構整理混為一談。
- 重新執行驗證，確認這次拆分沒有回歸。
- 針對 dependency review 補充實務結論：不再強行追逐 indirect duplicate，改為維持 direct dependency 收斂並讓 upstream 升級自然跟進。

### 驗證結果

- `cargo test --quiet`：104 passed, 0 failed

### 依賴收斂判定（補充）

- `direct dependency`：已經相當乾淨，沒有明顯未使用 crate；`iced`、`tokio`、`windows-sys`、`tracing-subscriber` 的 `default-features = false` / 最小 feature 集已是合理設計。
- `build-dependencies`：`winresource`、`png` 均為必要；`png` 目的在避免更重的 `image` crate，設計上是合理的收斂。
- `indirect dependency`：`windows-sys`、`syn`、`hashbrown`、`getrandom` 的重複多為上游 ecosystem 鎖定，不是本專案內部造成的直接依賴問題。
- 後續策略：維持 direct dependency 的最小化，避免為 API 方便再引入新 crate；對 indirect duplicates 只採 upstream-aware 處理，不再投入高風險 local workaround。

## 第十二輪：重複抽象 / 依賴去重 review（2026-08-12）

### 目標

- 依據「不要引入重複類型的 lib」之原則，重新檢查專案是否還存在 duplicate abstraction / wrapper-crate 問題。
- 區分可控的 direct dependency 與不可控的 upstream transitive duplicate。
- 把 review 結果寫回同一份 backlog，避免再繼續做低回報的依賴重構。

### 結論

- 本專案目前沒有明顯的「重複類型 library」問題：沒有兩個 direct crate 同時做同一功能、也沒有因為 API 方便而再包一層額外抽象。
- `iced`、`tokio`、`tracing`、`windows-sys`、`dirs`、`toml`、`serde` 都各自保持單一責任，並且彼此沒有明顯功能重疊。
- 真正的 duplicate 仍然集中在 transitive layer：`winit` / `windows-sys`、`framework_lib` / `winreg`、`syn`、`hashbrown`、`getrandom`，這些多為上游版本交叉鎖定，而非本 repo 自行引進。
- 新一輪 review 的最重要結論是：若不想引入重複類型 lib，應該限制在「不新增第二套 API wrapper」這個層面；對 transitive duplicate，採用 upstream-aware / version-driven 處理，而不是本地硬修。

### 具體判定

- **A. 可接受 / 已正確處理**
  - `iced` + `canvas` + `wgpu` 組合仍是單一 UI stack，沒有重複 GUI abstraction。
  - `tokio` 是唯一 async runtime，沒有額外獨立 async lib。
  - `tracing` / `tracing-subscriber` 是唯一 logging stack，不再另包 logger wrapper。
  - `windows-sys` 只用於必要 Win32 API，沒有額外 Windows 封裝 crate。
  - `toml` + `serde` 仍是 config 讀寫的單一方案。
- **B. 需要避免的方向**
  - 不再為了「寫法簡潔」而新增另一層 helper crate 包住 `iced`、`tokio`、`windows-sys` 或 `dirs`。
  - 不再引入額外 config / logging / path / ABI wrapper，除非它直接替代一個絕對必要且未被現有 lib 覆蓋的責任。
- **C. 仍屬 external constraint**
  - `windows-sys 0.52`（`winit`）、`windows-sys 0.59`（`winreg`）
  - `syn 2.0` vs `syn 3.0`
  - `hashbrown` 多版本
  - `getrandom` 多版本
  - 這些仍然是上游依賴樹的交叉問題，不應拖進本專案的 direct dependency 收斂策略裡。

### 實務建議

1. 直接依賴仍維持單一責任原則：一個功能只保留一個 crate。
2. 禁止新增「只是另一種 API 包裝」的 wrapper crate。
3. 任何新 dependency 都要先問：它是必要功能，還是另一個同類別薄封裝？
4. 對 indirect duplicate，採用 version 升級或 upstream patch，而不是本地對抗式 workaround。
5. 若日後需要新的功能，優先從既有 stack 直接擴充，不再引進重複抽象層。

### 驗證結果

- `cargo tree -d`：已確認剩餘的 duplicate 為 upstream transitive lock-ins，不是本 repo 自行新增的重複型別 lib。
- `cargo test --quiet`：104 passed, 0 failed

## 第十三輪：最終一致性檢查（2026-08-12）

### 目標

- 完成最後一輪一致性檢查，確認前述「單一責任 / 不新增重複抽象」原則是否已在程式碼與依賴設計中落地。
- 避免再為 transitive upstream constraint 進行本地化、風險較高的 workaround。
- 將最終結論寫回 backlog，確保後續新增依賴時仍遵守相同規範。

### 結論

- 本輪確認目前專案沒有需要再做大規模 direct dependency 收斂的明顯缺口；`Cargo.toml` 的直接依賴已近合理下限。
- 目前仍有價值的變動，僅限於 hot-path 的責任拆分、config save 的邊界整理，以及避免過度抽象化，而不是再去「減少 crate 數量」本身。
- 任何新依賴都應先問：它是必要功能，還是另一層 API wrapper？若屬於後者，應直接拒絕。
- 對 `windows-sys`、`syn`、`hashbrown`、`getrandom` 等 transitive duplicate，仍維持 upstream-aware 方針，不再做本地硬修。
- 結論：專案在依賴治理上已達到穩定狀態，接下來的主要工作是維持既有 hot-path 優化與防止新抽象層污染設計。

### 驗證結果

- `cargo test --quiet`：104 passed, 0 failed

## 第十四輪：最終維護性清理（2026-08-12）

### 目標

- 進行最後一輪低風險清理，消除現有 lint / 可讀性噪音。
- 保持 hot path、依賴收斂與資源效率策略不退化。
- 以最小變更保留目前已驗證的設計，不再投入高成本重構。

### 結論

- 目前已無明顯的高價值修正點；剩餘事項大多為可讀性與維護性整理，而非功能性或效能性問題。
- 本輪已完成兩個明確且低風險的 cleanup：
  - `src/app.rs` 中多處不必要的 `return Some(Task::none())` / `return Some(...)`，保留行為不變並消除 `clippy::needless-return`。
  - `src/background_task.rs` 抽出 `mark_view_dirty()`，讓各處 `view_dirty` 更新保持一致，減少重複邏輯。
- 這一輪的核心原則是「不為格式而重構，不為 lint 之名做大改」，維持目前穩定的依賴與性能架構。
- 目前的 backlog 已經從「發現 bug」轉為「防止回歸 + 保持整理」，這是合理終點。

### 已採取動作

- `src/app.rs`：移除不必要 `return`，保持 message handler 行為同值等價。
- `src/background_task.rs`：抽出統一 helper，集中管理 `view_dirty` 更新點。
- 以現有已驗證設計為主，不再引入新抽象層或額外依賴。

### 驗證結果

- `cargo clippy --all-targets -- -D warnings`：passed
- `cargo test --quiet`：104 passed, 0 failed

### 最終判定

- 本專案目前已達穩定狀態；若要再做下一步，應該只做架構維護性整理，而不是重複的 hot path 重構。
- 除非有明確外部需求，否則目前不建議再投入額外的依賴精簡或性能微調。

## 依賴狀態（2026-08-11）

`cargo outdated`：**All dependencies are up to date, yay!**

**直接依賴**（13 個，全部最新）：
| 套件 | 版本 | 用途 |
|------|------|------|
| `iced` | 0.14.0 | GUI 框架（wgpu/tokio/canvas/image/advanced） |
| `tokio` | 1.53.1 | async runtime |
| `serde` | 1.0.229 | serialization |
| `toml` | 1.1.4 | config 檔案讀寫 |
| `framework_lib` | 0.6.5 | Framework 筆電 EC/SMBIOS API |
| `tracing` | 0.1.44 | logging |
| `tracing-subscriber` | 0.3.23 | log formatting |
| `dirs` | 6.0.0 | config/data 目錄路徑 |
| `core_affinity` | 0.8.3 | CPU affinity |
| `parking_lot` | 0.12.5 | fast mutex/rwlock |
| `windows-sys` | 0.61.2 | Windows API |
| `winresource` | 0.1.31 | Windows EXE icon（build-dep） |
| `tempfile` | 3.27.0 | temp files（dev-dep） |

**間接依賴重複**（無法自行修復）：
| 重複 | 鎖定者 | 需要發生的事 |
|------|--------|-------------|
| `windows-sys 0.52` | winit 0.30 | winit 0.31 stable |
| `windows-sys 0.59` | winreg 0.55（framework_lib） | framework_lib 更新 winreg |
| `syn 2.0` vs 3.0 | 數十個 proc-macro crate | 生態系遷移到 syn 3.0 |
| `hashbrown 三版本` | gpu-descriptor/wgpu/naga/indexmap | 上游採用新版 |
| `getrandom 0.2` | redox_users | redox_users 遷移 |

## 專案背景

Framework 筆電控制 GUI（「Framework Crate」），Rust + Iced 0.14 + `framework_lib` v0.6.5。
目標：降低 CPU / RAM / GPU 使用率，先修掉 code review 發現的效能問題（#1–16），
再做記憶體（M 系列），最後做 CPU/GPU（G 系列）優化。

## 關鍵技術限制（踩過的坑）

- `framework_lib` 是同步 API，async context 必須用 `tokio::task::spawn_blocking`
- Iced 0.14 canvas `Program::update` 回傳 `Option<Action<Message>>`
- `canvas::Text::content` 是 `String`，不能借用 `&str`；`.title()` 需要 `Fn(&App) -> String`
- **`canvas::Geometry` 不實作 `Clone`** → 快取必須用 iced 內建 `canvas::Cache<Renderer>`（在 bounds 或資料變更時才重畫）
- `TempSample.temps` 是 `Arc<BTreeMap<String, i32>>`；`SensorCache.keys` 是 `Vec<String>`
- `temp_history` 已從 `Vec` 改為 `VecDeque<TempSample>`（`.last()` → `.back()`）
- `UsbCPort.power_role`/`data_role` 為 `Option<&'static str>`（消除每次 clone）
- `Arc::make_mut` 在共享引用存在時一定會整份 clone → 一般改用 **clone + replace** 模式（B2 例外：`temp_history` 唯一持有者為 `RwLock<Arc<..>>`，refcount=1 時 `make_mut` 零複製，已採用）
- `config::save()` 內部的 config clone 是必要的（validate + sort 會修改）且已 debounce，**保留**

## 已完成工作

### 第一輪：Critical 修正（#1–5）

- `sorted_sensor_list` O(n²) → 預建 HashMap lookup（`types.rs`）
- `CurveStepper::next()` 簽名改為純量參數 `(temp, hysteresis_c, rate_limit_up, rate_limit_down, full_points)`，消除每次重組 `CurveConfig`
- 10 個 `curve_stepper` 測試同步更新為純量參數
- 新增 `ViewSnapshot`（`views.rs`）：每 frame 只做一次 10 個 field 的 `Arc::clone`，子 view 全部改收 `&ViewSnapshot`
  - `view_fan_control`、`view_misc` 不再收 `&App`
- `AppState::clone()` ×4 → 只 clone 需要的欄位（ec_client, kblight）
- Init task `state.clone()` → `Arc::clone(&state.versions)`

### 第二輪：修正 #6–16

- `config_save_task.rs`：移除 `state.clone()` ×3，直接 `Arc::clone(&state.bg_config_save_failed)`、`Arc::clone(&config_rx.borrow())`
- `background_task.rs`：`Arc::make_mut` → clone + replace（`temp_history`、`pd_ports_history`）
- `refresh_all_data`：5× `state.clone()` → 5 個針對性 `Arc::clone`（battery、kblight、pd_ports+history、expansion_cards）
- `views.rs`：sensor settings 迴圈減少 String clone；`chart_colors.get(idx)` 防 OOB panic
- `ec_wrapper.rs`：`power_role`/`data_role` 改 `Option<&'static str>`

**結果：** `cargo check` 0 錯誤、`cargo clippy` 0 警告、98 tests 全過

### 第三輪：記憶體優化（M 系列）

- `temp_history`：`Arc::make_mut`（因共享引用必整份 clone）→ 顯式 `(**hist).clone()` → push/pop → `*hist = Arc::new(h)`，舊 Arc 立即釋放
- `push_pd_ports_history`：同樣模式
- `refresh_all_data`：每個 spawn_blocking 只 clone 需要的 Arc 欄位

### 第四輪：CPU/GPU 優化（G 系列）

- **Dirty-flag view 快取：**
  - `AppState` 新增 `view_dirty: Arc<AtomicBool>`（init `true`），背景任務每次資料寫入後設為 true
  - `view_main()`：dirty 時建 `ViewSnapshot` 存到 `App.cached_snapshot` 並清旗標；clean 時直接 clone 快取，省下 10 個 read lock + 重組成本
  - `ViewSnapshot` derive `Clone`；`App.cached_snapshot: RefCell<Option<ViewSnapshot>>`（`pub(crate)` 修 `private_interfaces` clippy 警告）
- **Geometry 快取（用 `canvas::Cache`，修正版）：**
  - `temp_chart.rs`：`TempChartRenderer` 改 `cache: Cache<iced::Renderer>` + `cached_key: RefCell<(*const (), usize, *const ())>`（samples Arc ptr + len + sensor_names Arc ptr）
  - `curve_canvas.rs`：`CurveRenderer` 同樣加 `cache` + key（all_pts Arc ptr + len + points Arc ptr）
  - 資料變更時 `cache.clear()` 強制重繪；bounds 不變 + 資料未變時直接回傳已存 geometry，CPU 繪圖與 GPU 上傳都省下
  - 繪圖邏輯抽出為自由函式 `draw_temp_chart_contents()` / `draw_curve_contents()`
- **Geometry 快取根本修正（G5，2026-08-11）：**
  - 查證 iced 0.14 原始碼確認：`Canvas` widget **沒有實作 `fn diff`**（`iced_widget-0.14.2\src\canvas.rs`），
    只有存進 widget `Tree` 的 `P::State` 會跨 `view()` rebuild 存活（`Tree::diff` 依 tag 保留，`iced_core-0.14.0\src\widget\tree.rs:63`）
  - 因此先前 `Cache` 放在 renderer（widget 欄位）裡、每次 `view()` 都 `Cache::new()` → **快取永遠不會 hit**，兩張 canvas 每 frame 全部重畫
  - 修正：`cache` + `cached_key` 移入 `Program::State`（`CurveState` / `TempChartState`，含 `OnceCell<Cache>` + `Cell<key>`，`draw()` 只收 `&State` 靠內部可變性）
  - `curve_canvas.rs` 額外 bug：`Arc::from(points)` 每 frame 新配置 → Arc ptr 每次都變 → key 永遠 mismatch → `cache.clear()` 每 frame。
    修正：key 只取 `(all_pts ptr, all_pts len)`（`update_curve_full_points()` 在每次控制點編輯後 100ms debounce 重算，`app.rs:737`），
    markers 變更則用 `state.last_points` 內容比對（`RefCell<Option<Arc<[[u32;2]]>>>`）
  - 驗證：`cargo check` 0 錯誤、`cargo clippy` 0 警告、98 tests 全過
- **未動（結論已確認不需改）：**
  - `config::save()` 內部 clone（必要，已 debounce）
  - `canvas::Text` 的 `.to_owned()`（iced API 限制）
  - `app_title` 回傳 `String`（iced API 限制）
  - RPM/duty 格式化已用 `String::with_capacity` + `write!`

**最終驗證：** `cargo check` 0 錯誤、`cargo clippy` 0 警告、98 tests 全過、0 compiler errors

### 第五輪：資源效率 code review + 第一波修正（2026-08-11）

- 全檔案 code review 完成（app.rs、background_task.rs、views.rs、tray/\*、config_save_task.rs、ec_wrapper.rs、config.rs、fan_control.rs、util.rs、main.rs、types.rs），
  以 4 個 subagent 平行讀檔 + 人工整理，輸出中文報告：A1/A2（功能性 bug）、B1–B6（中成本）、C1–C14（便宜修正）、D（已最佳化不需動）
- 使用者選擇第一波範圍：**A1/A2 + B3 + C1–C14**（B1 sub-view `Element<'_, Message>` 大改、B2 temp_history 重構暫緩）
- 完成項目：
  - **A1（高）** tick 鏈中斷：最小化到 tray 路徑原本只回傳一次性的 `MinimizeToTray` task，tray 事件永不輪詢 →
    改 `Task::batch([MinimizeToTray, tick_task(next_ms)])` 維持 tick 鏈（app.rs:347）
  - **A2（高）** EC panic 重連路徑不可達：`background_task.rs` 原本 `if !cli_available { continue }` 在重連邏輯之前 →
    改為只在大迴圈每次迭代都讀 ec_client、None 時直接嘗試重建（`CrosEc::new` 成功才 store），失敗才 continue
  - **B3** tree 結構穩定：warning banner ×2 與 sensor settings 面板改為「永遠保留 slot」，
    banner 用 `container(space())`、面板用空 `container(column![])` 佔位，避免子樹 Tree state（canvas 快取、scroll offset）在條件顯示切換時流失
  - **C1** 移除 `background_task.rs` 每輪死分配 `curve.points.clone()` 與死讀取 `curve_poll`
  - **C2** `record_thermal_sample` 回傳 `bool`（資料真正改變才設 `view_dirty`），僅熱樣本改變觸發重繪
  - **C3** 啟動 `refresh_all_data` 5 個 `spawn_blocking` 改 `tokio::join!` 平行執行（原本串行 ~500ms+）
  - **C4** `view_curve` 每 frame `Arc::from(points)`（會讓 G5 快取永不清到）→ `POINTS_ARC_CACHE`：
    以 `(points.as_ptr() as usize, len)` 為 key 復用上次 Arc，儲存不變時零配置、canvas 快取可命中
  - **C5** `points_buf` 從 renderer（每次 rebuild 新 Vec）移入 `TempChartState`（存活於 widget Tree）
  - **C6** `SensorToggled(String, bool)` → `SensorToggled(usize, bool)`（views 用 index，不再每 frame clone 名稱）；
    `FpLedLevelChanged(String)` → `FpLedLevelChanged(&'static str)`（`"low"`/`"medium"`/`"high"` 字面量）
  - **C7** `cached_snapshot`：dirty 時直接把新 snapshot move 進快取再借用（省一次整份 clone）；非 dirty 持 Ref 不 clone
  - **C8** `update_curve_full_points` 先比對 `last_curve_points` 內容，未變則不重建（避免 config 改動觸發快取失效）
  - **C9** `IsIconic` 檢查移入 5 秒 HWND 驗證區塊（原本每 tick 查）
  - **C10** tray icon 建立加 `icon_create_in_flight` flag，`MinimizeToTray`/tick 只呼叫一次 `check_icon_ready()`，避免重複 `CreateIcon`
  - **C11** `config_save_task` 啟動不再立即寫檔（設定剛從磁碟載入），第一次實際變更後才存
  - **C12** `config::save_fast()`（跳過 fsync）供 debounce 熱路徑使用；退出時 `save_config_now()` 保留全同步
  - **C13** tray message pump 退出時清除 `TRAY_THREAD_ID`（避免對已死執行緒 post message）
  - **C14** `thermal()` 的 sensor 名稱改 `SENSOR_NAMES` OnceLock 快取（keyed by platform），熱路徑每次 poll 只 clone 一次
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、98 tests 全過
- **Tray 還原白畫面修正（2026-08-11）：**
  - 根因：隱藏期間（SW_HIDE）視窗收不到 WM_PAINT（winit `RedrawWindow(RDW_INTERNALPAINT)` 對隱藏視窗無效）→ 隱藏時完全不 present；
    還原時第一個 present 可能拿到過期/失效 swapchain（0×0 或 DWM 已丟棄 surface）→ 一兩幀白畫面
  - 且隱藏時 tick 間隔 5000ms → tray 點擊最多等 5 秒才被輪詢到（視覺上就是「卡」）
  - 修正：`UI_HIDDEN_INTERVAL_MS` 5000→500（隱藏時無 present/view 成本，2Hz tick 近乎免費）；`RestoreFromTray` 強制 `view_dirty=true`（首幀用最新 snapshot）
  - **用戶回報：白畫面仍在，只是更快跳過** → 證明 App 側修正不足，根因是隱藏期間 swapchain 已失效
  - **第二階段：改「隱藏」策略為 off-screen parking（`src/system_info.rs`）：**
    - 不再 `SW_HIDE`（收不到 WM_PAINT → 無 present → swapchain 失效），改為把視窗移到 (-32000,-32000) 且保持 WS_VISIBLE
    - `hide_window_to_tray()`：`GetWindowPlacement` 存位置 → `SetWindowPos` 移到螢幕外 → 加 `WS_EX_TOOLWINDOW` 移除工作列/Alt-Tab → `SetFocus(null)`
    - `restore_window_from_tray()`：清 `WS_EX_TOOLWINDOW` → 還原已存位置（`SetWindowPlacement` SW_RESTORE，保留最大化狀態）→ `SetForegroundWindow`
    - 兩個函式切換 extended style 後都 `SetWindowPos(SWP_FRAMECHANGED)` 讓工作列重新評估
    - 效果：隱藏期間 WM_PAINT 持續送達（2Hz tick 仍有 present）→ swapchain 維持有效 → 還原第一幀即有內容，無白畫面
    - 成本：隱藏時仍有 ~2Hz present（比 SW_HIDE 的 0 present 多一點，可接受）
    - `tray/mod.rs` 的 `hide_window`/`restore_window` 改呼叫上述函式；移除未使用的 `show_window`/`set_foreground_window`
    - **踩坑紀錄（實測驗證，非猜測）：**
      1. `WINDOWPLACEMENT` 結構必須含保留欄位 `rcDevice`（共 60 bytes）— `SetWindowPlacement` 驗證 `length == sizeof`，缺了會失敗；
         已加編譯期斷言 `size_of::<WINDOWPLACEMENT>() == 60`
      2. **`SetWindowPlacement` 會把負座標夾回虛擬螢幕原點 (0,0)**（實測：park 後視窗跑到 (0,0)）→ 必須用 `SetWindowPos`
         移出螢幕；`SetWindowPos` 實測可以停在 (-32000,-32000)
      3. winit 0.30 預設 `WS_EX_APPWINDOW`（強制工作列按鈕），parking 時需一併清除，還原時加回，否則「工作列按鈕 + tray 圖示」雙層介面
    - 新增實測 `system_info::tests::parking_moves_window_offscreen_and_restores`（建立真實視窗 → park → 驗證離螢幕 → restore → 驗證還原）
    - 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、99 tests 全過

### 第六輪：第二波優化 B1/B2/B4（2026-08-11）

- 使用者確認執行第二波（B1 sub-view 借用、B2 temp_history 重構、B4 sensor_color 線性掃描）
- **B1：sub-view 全面改 `Element<'_, Message>` 借用**
  - 先決條件：原本 `ViewSnapshot` 存在 `App.cached_snapshot: RefCell<Option<ViewSnapshot>>`，`view()` 持 Ref guard → 回傳值借用 local guard（E0515）
  - 重構：snapshot 改為 `App.cached_snapshot: Option<ViewSnapshot>` 純欄位，重建移到 `update()` 開頭
    （`view_dirty || is_none()` 時重建並清旗標）；`InitComplete` handler 直接建 snapshot，首幀不會空白
  - `view_main` 直接 `match &app.cached_snapshot`（存活於 `&self`），`None` 時回傳 defensive placeholder
  - 子 view 移除全部每 frame `Arc::clone(&snap.*)`：改 `&snap.*` 借用；`text(name.clone())` → `text(name.as_str())`
  - 改動：`view_sensors`/`view_battery`（需 `<'a>` 兩個參數）、`view_fan_control`/`view_battery_info`/`view_battery_verbose`/
    `battery_detail_rows`/`view_misc`/`kblight_section`/`ports_section`（單參數 `'_`）
  - 行為等價：`view_dirty` 語意保留，背景寫入後下一個 message 即重建 snapshot
- **B2：temp_history 雙緩衝重構**
  - 新增 `temp_chart::ThermalHistory { draft: VecDeque<TempSample>, published: Arc<VecDeque>, last_publish_ms }`
    （`#[derive(Clone)]`，內部可變性由外層 `RwLock<Arc<..>>` 提供）
  - `push_sample(&mut self, sample, now_ms)`：push + cutoff 清理（`HISTORY_MS`）
  - `snapshot(&mut self, now_ms)`：每 `HISTORY_PUBLISH_MS`(1s) 才整份 clone 發佈，其餘回傳 `Arc::clone(&published)`
  - `AppState.temp_history` 型別：`Arc<RwLock<Arc<VecDeque<TempSample>>>>` → `Arc<RwLock<Arc<ThermalHistory>>>`
  - writer（`record_thermal_sample`）：`Arc::make_mut(hist).push_sample(...)`（refcount=1 時零複製，取代原本每 sample 整份 clone）
  - reader（`ViewSnapshot::from_app`）：`with_write_lock(... |h| Arc::make_mut(h).snapshot(now_ms))`
  - 新增 5 個 `temp_chart::tests`：empty、prune cutoff、首發即時、interval 內 Arc 重用（`Arc::ptr_eq`）、interval 後重發佈
- **B4：sensor settings `sensor_color()` 線性掃描**
  - 迴圈已是 `for (idx, name) in cache.keys.iter().enumerate()`，直接用 `SENSOR_COLORS[idx % len]`（與 `sensor_color()` 邏輯等價，省掉每 row `position()` 掃描）
  - `sensor_color()` 保留給 cache 建立熱路徑（`background_task.rs:143`、`app.rs:827`）使用
- **B6：sensor settings 面板 `contains()` 每 frame 查**
  - 原本每列 `config.telemetry.selected_sensors.contains(name)` 是 O(n) 線性掃描，面板每 frame 重建時重複 O(n²)
  - 改為迴圈前預建 `HashSet<&str>`（`selected_sensors.iter().map(|s| s.as_str()).collect()`），每列改 O(1) `selected_set.contains(name.as_str())`
  - `all_empty`（全選）時不建 set、直接略過檢查
- 實機驗證（使用者）：tray 最小化/還原（parking）、sensor 開關、電池頁面均正常
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、104 tests 全過（+5）

### 第七輪：視窗高度自動貼齊內容 + Misc scroll bar（2026-08-11）

- **HeightProbe 自訂 widget**（`src/probe.rs`）：透明包裝 widget，layout 時量測內容實際高度寫入 `App.content_height: Arc<Mutex<Option<f32>>>`，委派其餘所有 Widget 方法（children/diff/draw/update/mouse_interaction/operate/overlay）
  - 踩坑：`draw`/`mouse_interaction`/`overlay` 必須傳 `&tree.children[0]`（child 的 tree），不能傳 parent tree（`Downcast on stateless state` panic）
  - `iced::advanced` 需在 Cargo.toml 加 `"advanced"` feature
- **視窗自動調整高度**（`src/app.rs`）：
  - `Message::WindowResized(Id, Size)` + subscription batch（`resize_events`）
  - `autosize_task()`：init 完成後首次 layout 即量測內容高度，加 25px 底部呼吸空間，上限 760 邏輯像素（避免 fan curve 模式撐出螢幕）
  - `height_set: bool` flag：高度設定一次後鎖定，展開/收合區塊（battery details、sensor settings）不改變視窗高度，改為內部 scroll
  - `update()` 拆為 wrapper + `update_inner()`，每次 message 後自動檢查是否需要 resize
- **修正 iced `.window()` 覆蓋問題**：iced 0.14 的 `.window(Settings{..})` 會覆蓋先前 `.window_size()`/`.resizable()` 的設定 →
  改將 `size`/`resizable` 寫入 `.window(Settings{..})` 結構內；`.resizable(false)` 鎖定不可拖曳
- **Battery & Power scroll bar**（`src/views.rs`）：`BATTERY_SECTION_MAX_HEIGHT: 300.0`，`scrollable(...).height(Shrink)` + `container.max_height(300)`
- **Misc scroll bar**（`src/views.rs`）：`MISC_SECTION_MAX_HEIGHT: 300.0`，同樣 pattern 包裹 kblight + fingerprint LED + ports
- **B6 實機驗證通過**（sensor settings 開關顏色正常）
- **全部 commit 已 push 到 GitHub**（`92da4cb..f25ae27`，含第一～七輪全部工作）
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、104 tests 全過

### 第八輪：依賴升級 + tray 還原修正（2026-08-11）

- **依賴升級**：`dirs` 5.0.1→6.0.0、`toml` 0.8.23→1.1.4
  - 消除間接依賴：`toml_edit 0.22`、`winnow 0.7`、`toml_write 0.1`、`toml_datetime 0.6`、`serde_spanned 0.6`、`windows-sys 0.48` + 9 個 windows-targets 子套件、`dirs-sys 0.4`、`redox_users 0.4`
  - `cargo outdated` 回報：All dependencies are up to date
  - 程式碼影響：零（API 簽名不變，`config_dir()`/`data_local_dir()`/`to_string()`/`from_str()` 行為完全相同）
  - 建置改善：依賴數量減少、binary size 略減、compile time 略快
- **Tray 還原標題列修正**（`system_info.rs`）：
  - 根因：`hide_window_to_tray` 設定 `WS_EX_TOOLWINDOW` 讓 DWM 用 tool window 樣式渲染（只有 X 按鈕），還原時移除 `WS_EX_TOOLWINDOW` 但 DWM 可能快取舊樣式
  - 修正：儲存/還原 `GWL_STYLE`（`SAVED_STYLE` static），確保 minimize/maximize 按鈕完整恢復
- **Clippy 修正**：`autosize_task` 的 match arm 改用 `?` 運算子
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、104 tests 全過
- 已 push：`f25ae27..db26218`

### 第九輪：EC/BIOS 讀值修正（2026-08-11）

- **UEFI BIOS 版本修正**（`src/cli/ec_wrapper.rs`）：
  - 根因：`fw_type == 0` 過濾不到正確的 SystemFirmware entry → 改為 `fw_type == 1`
  - 格式：`{:02X}.{:02X}`（後兩個 byte，如 `03.05`），取代原本 4 byte 顯示
- **EC Firmware 版本顯示修正**（`src/cli/ec_wrapper.rs`）：
  - 根因：`flash_version()` 回傳 RO/RW 兩個版本字串 + 當前 active image
  - 修正：根據 `EcCurrentImage` 判斷，只顯示當前 active 版本（RO 或 RW），不再顯示 `RO:xxx RW:yyy Current:RO` 格式
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、104 tests 全過
- 已 push：`db26218..33cb503`

### 第十輪：架構重構 + 依賴清理（2026-08-12）

- **AppState 拆分為 6 個 sub-state struct**（`src/sub_state.rs`）：
  - `FanState`：mode、curve_poll_ms、last_applied_duty、fan_max_rpm、last_fan_rpm_reset、curve_full_points
  - `ThermalState`：data、history、sensor_cache
  - `PeripheralState`：kblight、expansion_cards、pd_ports、pd_ports_history
  - `BatteryState`：info
  - `SystemState`：cli_available、ec_client、versions
  - `LifecycleState`：config、poll_ms、shutdown、visible、last_interaction_ts、bg_config_save_failed、view_dirty
  - 更新所有引用（app.rs、background_task.rs、config_save_task.rs、main.rs、views.rs）
- **Lock helpers 移至 `src/util.rs`**：`read_lock()`、`with_write_lock()` 從 app.rs 搬出
- **`mutate_config` 輔助方法**（`src/app.rs`）：封裝 `Arc::make_mut` + write lock 模式，替換 12 處重複 call site
- **`run_ec_task` 輔助方法**（`src/app.rs`）：簡化 EC 任務執行（kblight、fp_led、autofanctrl、set_fan_duty）
- **Fan duty 範圍擴展至 0–100%**：原本最低 10%，現在支持 0%（完全停轉）
- **邊界檢查強化**：
  - `FanControlMode::from_u8`：明確處理 0=Disabled，未知值 warn
  - Curve temp 範圍 0–99、duty 範圍 0–100
  - `saturating_sub` 替代 `sub` 防止 hysteresis 比較溢位
- **依賴清理**：
  - 移除 `serde_json`（未使用）
  - 移除 tokio features：`io-util`、`fs`（未使用）
  - 移除 windows-sys features：`Win32_Foundation`、`Win32_System_LibraryLoader`（未使用）
  - 移除 `BatteryData`、`BatteryInfo` 上未使用的 `serde::Serialize/Deserialize` derive
- **`#[cfg(debug_assertions)]`** 加在 `verify_affinity`（背景任務）
- **Commit message 格式統一**：全部改為 Title Case，比照 `fc9d495` 格式（含 bullet points）
- 驗證：`cargo check` 0 錯誤、`cargo clippy --all-targets` 0 警告、104 tests 全過
- 已 push：`33cb503..1cfb87d`

## 目前狀態 / 進行中

- **第一波（A1/A2/B3/C1–C14）已完成並驗證**（見第五輪）
- **第二波（B1/B2/B4）已完成並驗證**（見第六輪）；**B6 已完成並實機驗證通過**（見第六輪、第七輪）
- **視窗自動貼齊內容**（HeightProbe + autosize_task）已完成並驗證（見第七輪）
- **Battery & Power / Misc scroll bar** 已完成（見第七輪）
- **依賴升級完成**（dirs 5→6、toml 0.8→1.1），所有依賴已是最新版（見第八輪）
- **Tray 還原標題列修正**（GWL_STYLE 儲存/還原）（見第八輪）
- **EC/BIOS 讀值修正**：UEFI BIOS 版本正確顯示（fw_type==1，XX.XX 格式）、EC Firmware 只顯示當前 active 版本（見第九輪）
- **架構重構完成**：AppState 拆分為 6 sub-state、lock helpers 搬至 util、mutate_config 輔助方法、依賴清理（見第十輪）
- 已確認不需要處理（D 區）：GetMessageW 阻塞泵、EC 只在 duty 變時寫、CurveStepper 零分配、sorted_sensor_list HashMap、
  curve_full_points debounce、classify_pd_port 零分配、dirty-flag snapshot、idle/hidden 降頻
- 注意事項：`SensorToggled` 空選時語意維持原邏輯（先全選再增刪）；`RestoreFromTray` 會重置 `icon_create_in_flight`

### 第十一輪：Code Review backlog + 依賴精簡 / 資源效率 review（2026-08-12）

- **A. 高優先度：配置寫檔競態（已改善，仍留最後一道收斂）**
  - `config::save()` / `save_fast()` / `save_config_task` 已經有 temp file + atomic replace、debounce 與 shutdown sync-save 的分層，這是明顯進步。
  - 但目前仍然存在多來源同時觸發保存的邏輯空間：UI event、background task、shutdown path 都可能在同一時間發出保存請求。
  - 重新建議：把 config save 收斂成唯一 writer，讓所有 save request 都經過同一條 pipeline；不再在 shutdown path 直接外插寫磁碟，避免「新快照 vs 舊快照」交錯。
  - 可行做法：保留 `config_tx` 作為唯一入口，讓 `save_config()`、`save_config_now()`、debounce 都走同一條保存序列。
- **B. 中優先度：`App::update()` 職責過重（仍是長期 backlog）**
  - `App::update()` 雖已拆出 `update_inner()` 與 helper，但其分支密度仍偏高，仍同時負責 timer、tray、window resize、EC command、config save、quit flow、UI toggle。
  - 重新建議：把它再拆成 `update_tick()`、`handle_tray_message()`、`handle_quit_message()`、`handle_ec_message()`、`handle_config_message()` 等 helper，讓責任更清楚。
  - 這一層的目標不是「再切得更碎」，而是避免後續新增設定頁或 tray 行為時牽一髮動全身。
- **C. 中優先度：close / minimize 行為語意需要明確化**
  - `CloseRequested` 目前直接走 `MinimizeToTray`，雖然已經和 tray/off-screen parking 配合得較好，但從使用者語意上仍然容易混淆「關閉」與「最小化」。
  - 重新建議：把視窗 close、tray hide、quit with restore 分層處理，並在界面或狀態中明確表達「目前是托管到系統匣」而非真正退出程序。
  - 這不只改善 UX，也能降低 lifecycle 與 app state 之間的耦合。
- **D. 中優先度：shutdown 路徑的一致性仍需加強**
  - `QuitWithoutRestore` / `QuitShutdown` 會同步寫檔，這是正確的最後保證，但 background task + config save order 尚未完全抽象成單一 event ordering。
  - 重新建議：在 shutdown 流程中保證最後 snapshot 一定落地，必要時增加 ack/flag，避免程序關閉前最後一次設定浮失。
  - 這部分建議採用「最後狀態寫出」而不是「任何快照都寫出」的策略。
- **E. 低優先度：維護性收斂（已開始進行，但還沒完全收斂）**
  - `App`、`views`、`tray`、`background_task` 之間的責任仍有重疊，尤其在 window lifecycle 與 user interaction dispatch 上。
  - 重新建議：後續依然採用責任分層：state/data model、UI rendering、interaction dispatch、background hardware I/O、window/tray lifecycle。
  - 這是長期架構整理，而不是本次的 urgent fix。
- **F. 直接依賴：已經收斂到合理範圍，方向正確**
  - [Cargo.toml](Cargo.toml) 中 `iced`、`tokio`、`windows-sys` 已使用 `default-features = false` + 最小 feature 集，這是對的。
  - `framework_lib` 是核心硬體依賴，現階段不宜過度刪除；但它的同步 API 還是要保持在背景 task 層，避免 UI polling 被硬阻塞。
  - `tracing-subscriber` 的初始化成本值得後續觀察，但目前不屬於當前高風險問題。
- **G. 間接依賴：已處理關鍵重複，餘下多為上游生態鎖定**
  - 已知重複仍存在：`windows-sys`、`syn`、`hashbrown`、`getrandom`，多半不是本專案直接造成，而是依賴樹上游版本交叉鎖定。
  - 目前最正確的策略仍是：保持直接依賴最小、避免新增無關依賴、讓上游升級自然跟進。
  - 也就是說，這不是「砍 dependency 數量」問題，而是「維持依賴收斂」問題。
- **H. 資源效率：已經是最成熟、最值得保留的成果**
  - `view_dirty` + cached snapshot 的設計有效壓低不必要 UI 重建成本。
  - `ThermalHistory` 雙緩衝 + 1s publish interval 有效避免 heat map/temperature chart 無謂 clone。
  - [src/temp_chart.rs](src/temp_chart.rs) 與 [src/curve_canvas.rs](src/curve_canvas.rs) 的 cache key + `Arc` reuse，符合 CPU/GPU 層的真實優化方向。
  - 這部分在評估中應被視為已落地成果，而不是 backlog。
- **I. 程式使用資源：持續觀察 lock / clone / redraw 三個關鍵面**
  - `AppState`、`background_task`、`views` 的 `Arc` / `RwLock` 圍繞方式，仍是效能風險的核心點。
  - 未來新增功能時，要優先檢查：是否在 hot path 產生 repeated clone、是否保留過長 lock 持有、是否在 frame 層做了不必要 redraw。
  - 這三個方向仍然比單點微調更值得持續優化。
- **J. 優先順序建議（修正版）**
  - 第一優先：保持 `default-features = false` + 最小 feature set 的收斂策略。
  - 第二優先：守住 hot path 的 cache / dirty flag / debounce / thermal history 這些已完成優化，不讓它退化。
  - 第三優先：再檢查 `framework_lib` / `tracing-subscriber` / window lifecycle 是否仍有不必要初始化或同步阻塞。
  - 第四優先：才處理 `App::update()` 的架構拆分與 config save 統一。
- **K. 最終判斷**
  - 相比上一輪 review，這一輪的重點已從「找 bug」轉成「確認哪些優化已經落地、哪些仍屬 backlog」。
  - 目前最值得保留的東西是：依賴精簡、dirty-flag snapshot、canvas cache、thermal history double buffer、off-screen parking 對 tray 還原的修正。
  - 仍需持續收斂的是：config save ordering、`App::update` 拆分、close/minimize lifecycle 一致性。
- **驗證：** `cargo test --quiet` 目前為 104 passed, 0 failed；這次 review 以現有實際 code 與驗證結果為基礎，結論保留 backlog，同時把依賴精簡與資源效率評估納入同一節。

## 下一步

1. ~~實機驗證 B6（sensor settings 開關顏色）~~ ✅ 已驗證通過
2. ~~確認 commit 並 push 到 GitHub~~ ✅ 已 push（`1cfb87d`）
3. ~~依賴升級~~ ✅ 所有依賴已是最新版
4. ~~Tray 還原標題列~~ ✅ 已修正
5. ~~EC/BIOS 讀值修正~~ ✅ 已修正並 push
6. ~~架構重構~~ ✅ AppState 拆分、mutate_config、依賴清理（見第十輪）
7. 若 parking 策略實機仍有問題，再考慮 keep-alive present 或合成器層級方案
8. ~~視窗高度可微調（修改 `+ 25.0` padding 值）~~ ✅ 已完成
9. ~~A（高）Expansion Card 分類修正~~ ✅ 修正 classify_pd_port() USB-C 不再誤判為 HDMI（見第十五輪）
10. ~~B（中高）平台功能篩選~~ ✅ PlatformFamily + feature matrix + Not Supported 淡紅色區塊（見第十五輪）
11. ~~C（中）Multi-Fan 支援~~ ✅ fan count 偵測 + Use Unified Duty toggle + per-fan sliders（見第十五輪）
12. ~~D（低）Debug 按鈕~~ ✅ About 頁 Collect Debug Info + 純文字 report（見第十五輪）

## 相關檔案

- `src\app.rs` — `App` struct、Message handlers、`mutate_config` 輔助方法、`run_ec_task` 輔助方法、`save_config()`、init task versions Arc；`content_height: Arc<Mutex<Option<f32>>>`、`window_id`、`window_height`、`height_set`（視窗自動貼齊）；`autosize_task()`（25px padding、760 max）；`update()/update_inner()` 拆分
- `src\sub_state.rs` — **新增** `FanState`、`ThermalState`、`PeripheralState`、`BatteryState`、`SystemState`、`LifecycleState`（AppState 拆分為 6 個 sub-state struct）；`PdPortsHistory` type alias
- `src\views.rs` — `ViewSnapshot`（10 Arc-clone、`#[derive(Clone)]`）、`view_main` 借用 snapshot、sub-views 全部 `Element<'_, Message>` 借用（B1）、sensor settings 索引取色（B4）
- `src\background_task.rs` — `record_thermal_sample` 用 `Arc::make_mut + push_sample`（B2）、`refresh_all_data` 針對性 Arc clone、各處 `view_dirty.store(true)`；更新所有 state 引用為 sub-state 路徑
- `src\config_save_task.rs` — 移除 `state.clone()`；更新 state 引用為 sub-state 路徑
- `src\types.rs` — `sorted_sensor_list` HashMap 版
- `src\fan_control.rs` — `CurveStepper::next()` 純量參數；`saturating_sub` 防溢位
- `src\cli\ec_wrapper.rs` — `UsbCPort` 的 `Option<&'static str>` roles；UEFI BIOS `fw_type==1` + `{:02X}.{:02X}` 格式；EC Firmware 只顯示當前 active 版本
- `src\temp_chart.rs` — `ThermalHistory` 雙緩衝（B2，`draft` + `published` Arc + 1s 發佈間隔）、`VecDeque`、`canvas::Cache` 快取（G5）
- `src\curve_canvas.rs` — `canvas::Cache` 快取（G5：cache 移入 `CurveState`；C4：`POINTS_ARC_CACHE` 復用 Arc）
- `src\config.rs` — `save()` 內部 clone 保留（必要，debounced）；C12：`save_fast()` 熱路徑跳 fsync
- `src\system_info.rs` — tray parking（off-screen + WS_EX_TOOLWINDOW）；`SAVED_PLACEMENT` + `SAVED_STYLE`（GWL_STYLE 儲存/還原修正標題列按鈕）
- `src\util.rs` — 時間工具；`read_lock()`、`with_write_lock()` lock helpers（從 app.rs 搬出）
- `src\tray\message_pump.rs` — C13：退出時清除 `TRAY_THREAD_ID`
- `src\main.rs` — `app_title` 不動；`mod sub_state`；`.window(Settings{size:900×613, resizable:false, ...})`（修正 iced `.window()` 覆蓋問題）
- `src\probe.rs` — **新增** `HeightProbe` 自訂 widget（layout 量高 + 委派 draw/update/mouse_interaction/operate/overlay）
- `Cargo.toml` — iced features 加 `"advanced"`（`iced::advanced` 需要）；`dirs = "6"`、`toml = "1"`（依賴升級）；移除 `serde_json`、tokio `io-util`/`fs`、windows-sys `Win32_Foundation`/`Win32_System_LibraryLoader`
