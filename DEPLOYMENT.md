# 部署與打包

## 建立單一 .exe

```bash
cargo build --release
```

輸出路徑：`target\release\framework-control-gui.exe`

圖示透過 `build.rs` + `winresource` 自動嵌入。

## 推薦方案：Inno Setup + winget

### Inno Setup 打包內容

建立 `installer.iss`：

```pascal
[Setup]
AppName=Framework Control
AppVersion=0.1.0
DefaultDirName={autopf}\Framework Control
DefaultGroupName=Framework Control
OutputDir=installer
OutputBaseFilename=setup
Compression=lzma2
SolidCompression=yes
; 需要管理員權限安裝（建立排程器任務）
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog

[Files]
; 只需打包主程式，Rust 靜態連結無額外 DLL
Source: "target\release\framework-control-gui.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; 開始功能表捷徑
Name: "{group}\Framework Control"; Filename: "{app}\framework-control-gui.exe"
Name: "{group}\Uninstall"; Filename: "{uninstallexe}"

[Run]
; 安裝後自動建立工作排程器任務（登入時以最高權限執行）
Filename: "schtasks.exe"; Parameters: "/create /tn ""Framework Control"" /tr ""\""{app}\framework-control-gui.exe\"""" /sc onlogon /rl highest /f"; Flags: runhidden

; 可選：安裝後自動啟動
Filename: "{app}\framework-control-gui.exe"; Description: "啟動 Framework Control"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 卸載時刪除排程器任務
Filename: "schtasks.exe"; Parameters: "/delete /tn ""Framework Control"" /f"; Flags: runhidden
```

### 打包命令

```bash
# 1. 建立 release exe
cargo build --release

# 2. 使用 Inno Setup 編譯
ISCC.exe installer.iss

# 輸出：installer\setup.exe
```

### Inno Setup 打包清單

| 檔案 | 說明 |
|------|------|
| `framework-control-gui.exe` | 主程式（靜態連結，無額外 DLL） |
| 排程器任務 | 安裝時自動建立，卸載時自動刪除 |
| 開始功能表捷徑 | 可選 |
| 解除安裝程式 | Inno Setup 自動產生 |

### winget Manifest

```yaml
PackageIdentifier: FrameworkControl.FrameworkControl
PackageVersion: 0.1.0
PackageName: Framework Control
Publisher: Framework Control
License: MIT
ShortDescription: Framework laptop fan control and telemetry GUI
Installers:
  - Architecture: x64
    InstallerType: inno
    InstallerUrl: https://github.com/YOUR_USER/framework-control-windows-Iced/releases/download/v0.1.0/setup.exe
    InstallerSha256: YOUR_SHA256_HASH
ManifestType: singleton
ManifestVersion: 1.0.0
```

### 發佈流程

1. `cargo build --release` 建立 exe
2. `ISCC.exe installer.iss` 建立 setup.exe
3. 上傳 setup.exe 到 GitHub Release
4. 計算 SHA256：`certutil -hashfile setup.exe SHA256`
5. Fork [winget-pkgs](https://github.com/microsoft/winget-pkgs)
6. 在 `manifests/f/FrameworkControl/FrameworkControl/版本/` 建立 YAML
7. 建立 Pull Request 等待審核（通常1-3天）

## 系統需求

- Windows 10/11
- 管理員權限（framework_tool 需要 EC 存取）
- Intel Core Ultra Series 1 (Meteor Lake) Framework 筆電
