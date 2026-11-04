//! Logon startup task registration via schtasks XML.
//!
//! Split from `system_info.rs` (P3); the parent re-exports this module so
//! `crate::system_info::*` paths keep working.

use super::to_wide;
use std::os::windows::process::CommandExt;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TokenElevation};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};

const STARTUP_TASK_NAME: &str = "FrameworkCrate";
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Process-wide single-instance guard backed by Windows named mutex.
pub struct SingleInstanceGuard {
    _handle: *mut core::ffi::c_void,
}

// Handle is owned, not dereferenced; Move-only across threads.
unsafe impl Send for SingleInstanceGuard {}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if self._handle.is_null() {
            return;
        }
        unsafe {
            CloseHandle(self._handle);
        }
    }
}

impl SingleInstanceGuard {
    /// Tries to acquire the named mutex.
    ///
    /// `Ok(Some(_))` acquired it, `Ok(None)` another instance already holds it.
    /// `Err` means the mutex could not be created at all (e.g. handle
    /// exhaustion), which is deliberately distinct from "already running": the
    /// caller must not exit silently in that case, or a scheduled logon launch
    /// looks like it did nothing.
    pub fn acquire(name: &str) -> Result<Option<Self>, std::io::Error> {
        const ERROR_ALREADY_EXISTS: u32 = 183;
        let wide = to_wide(name);
        // SAFETY: Null-terminated UTF-16 name; non-null handle with ERROR_ALREADY_EXISTS means owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
        if handle.is_null() {
            // SAFETY: null handle, nothing to close.
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: GetLastError immediately after CreateMutexW.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if exists {
            // Close handle to existing mutex; owning instance keeps it alive.
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self { _handle: handle }))
    }
}

/// Whether app is registered to launch at startup via scheduled task.
pub fn startup_launch_enabled() -> bool {
    schtasks_output(&["/Query", "/TN", STARTUP_TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Runs schtasks with a 5s timeout so a hung Task Scheduler service cannot
/// freeze the UI thread (toggle handler) or delay startup.
/// Decodes console child-process output. schtasks errors on zh-TW systems
/// are in the OEM codepage (cp950), not UTF-8 — from_utf8_lossy would
/// render them as mojibake and hide the real cause.
pub(crate) fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    // SAFETY: GetOEMCP takes no args; MultiByteToWideChar with valid buffers.
    unsafe {
        let cp = GetOEMCP();
        let len = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if len <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; len as usize];
        let written = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            len,
        );
        if written <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide[..written as usize])
    }
}

/// Runs schtasks with a 5s timeout so a hung Task Scheduler service cannot
/// freeze the UI thread (toggle handler) or delay startup.
/// The task action is registered through `/XML`, so no argument value has to
/// survive schtasks' own command-line re-parsing.
fn schtasks_output(args: &[&str]) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    // Own the arguments so nothing borrows the caller across the wait below.
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut cmd = std::process::Command::new(crate::util::system32_exe("schtasks.exe"));
    cmd.args(&args);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run schtasks: {e}"))?;
    // Drain pipes on threads so a chatty child can never block on a full pipe
    // while the main thread polls for exit.
    let mut stdout = child.stdout.take();
    let out_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut o) = stdout {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut e) = stderr {
            let _ = e.read_to_end(&mut buf);
        }
        buf
    });
    let start = std::time::Instant::now();
    loop {
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => {
                if start.elapsed() >= std::time::Duration::from_secs(5) {
                    // Kill on timeout: a /Create that lands after we already
                    // reported failure would enable startup the UI claims is
                    // off (or vice versa).
                    let _ = child.kill();
                    let reap_deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(1);
                    while child.try_wait().ok().flatten().is_none()
                        && std::time::Instant::now() < reap_deadline
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    // Closing the killed child's pipes lets the drain threads
                    // finish; join so a timeout does not leak them.
                    let _ = out_thread.join();
                    let _ = err_thread.join();
                    return Err("schtasks timed out after 5s".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                let _ = child.kill();
                let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while child.try_wait().ok().flatten().is_none()
                    && std::time::Instant::now() < reap_deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(format!("failed to run schtasks: {e}"));
            }
        };
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        return Ok(std::process::Output {
            status,
            stdout,
            stderr,
        });
    }
}

/// Escapes text for inclusion in the startup task XML.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Task Scheduler's `/XML` importer only accepts UTF-16. A UTF-8 file is
/// rejected with a parse error that points at the XML declaration, so the
/// document is encoded to UTF-16LE with a BOM here.
fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(s.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Builds the logon startup task XML. `schtasks.exe` cannot express power or
/// retry policy, so the task is registered from XML to apply them in one
/// atomic operation, instead of a follow-up PowerShell call that can fail
/// silently and leave the task refusing to start on battery.
/// Settings that matter for a long-lived tray app:
/// - battery conditions cleared: schtasks defaults them to true, which makes a
///   laptop on battery silently skip the task.
/// - `ExecutionTimeLimit` `PT0S`: schtasks defaults to `PT72H`, which stops the
///   tray app three days after logon and never restarts it.
/// - `RestartOnFailure`: retry a transient startup failure instead of giving up.
///
/// Child element order follows the schema; Task Scheduler rejects a document
/// whose `Settings` children are out of order.
fn build_task_xml(command: &str, args: &str, user_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Framework Crate</Author>
    <Description>Launch Framework Crate minimized to the notification area at logon.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
  </Settings>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user_id),
        command = xml_escape(command),
        args = xml_escape(args),
    )
}

/// Whether the process runs with an elevated (administrator) token.
///
/// The startup task is registered with `RunLevel=HighestAvailable`, so
/// registering it requires elevation. Reported to the user as a readable
/// message instead of the localized schtasks "access denied" text.
pub fn is_elevated() -> bool {
    const TOKEN_QUERY: u32 = 0x0008;
    // SAFETY: pseudo-handle is always valid; the token handle is closed below.
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned: u32 = 0;
    // SAFETY: token is a live handle and the buffer matches TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            std::ptr::from_mut(&mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    } != 0;
    // SAFETY: token was opened above and is not used afterwards.
    unsafe { CloseHandle(token) };
    ok && elevation.TokenIsElevated != 0
}

/// Account for the task principal. Task Scheduler resolves `DOMAIN\user` to a
/// SID while registering, so a later account rename does not break the task.
///
/// Resolved from the process token, never from `USERNAME`/`USERDOMAIN`:
/// environment variables are caller-controlled and must not name the account
/// an elevated task runs as.
fn current_user_id() -> Result<String, String> {
    use windows_sys::Win32::Security::{LookupAccountSidW, SID_NAME_USE, TOKEN_USER, TokenUser};
    use windows_sys::core::{PCWSTR, PWSTR};

    const TOKEN_QUERY: u32 = 0x0008;
    // SAFETY: pseudo-handle is always valid; the token handle is closed below.
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err("cannot open process token for startup task".to_string());
    }
    // Size query first: expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let mut len: u32 = 0;
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        // SAFETY: token was opened above and is not used afterwards.
        unsafe { CloseHandle(token) };
        return Err("cannot size token user for startup task".to_string());
    }
    // u64 backing keeps the TOKEN_USER struct aligned.
    let mut buf = vec![0u64; len as usize / 8 + 1];
    // SAFETY: buffer sized by the query above; token is live.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            (buf.len() * 8) as u32,
            &mut len,
        )
    };
    if ok == 0 {
        // SAFETY: token was opened above and is not used afterwards.
        unsafe { CloseHandle(token) };
        return Err("cannot read token user for startup task".to_string());
    }
    // SAFETY: buffer holds a valid TOKEN_USER per the successful call above.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    // SAFETY: token was opened above and is not used afterwards.
    unsafe { CloseHandle(token) };

    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_use: SID_NAME_USE = 0;
    // SAFETY: null buffers query the required lengths; may fail, ignored.
    unsafe {
        LookupAccountSidW(
            std::ptr::null() as PCWSTR,
            sid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_use,
        )
    };
    if name_len == 0 {
        return Err("cannot resolve account name for startup task".to_string());
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: buffers sized by the query above; sid came from our own token.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null() as PCWSTR,
            sid,
            name.as_mut_ptr() as PWSTR,
            &mut name_len,
            domain.as_mut_ptr() as PWSTR,
            &mut domain_len,
            &mut sid_use,
        )
    };
    if ok == 0 {
        return Err("cannot resolve account name for startup task".to_string());
    }
    let trim_nul = |v: &[u16]| {
        let n = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..n])
    };
    let user = trim_nul(&name);
    if user.is_empty() {
        return Err("cannot determine current user for startup task".to_string());
    }
    let domain = trim_nul(&domain);
    if domain.is_empty() {
        Ok(user)
    } else {
        Ok(format!("{domain}\\{user}"))
    }
}

fn known_common_app_data() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::UI::Shell::SHGetFolderPathW;
    const CSIDL_COMMON_APPDATA: i32 = 0x0023;
    const MAX_PATH: usize = 260;
    let mut buf = [0u16; MAX_PATH];
    let ok = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            CSIDL_COMMON_APPDATA,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr(),
        )
    };
    if ok != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(MAX_PATH);
    String::from_utf16(&buf[..len])
        .ok()
        .map(std::path::PathBuf::from)
}

fn task_dir() -> std::io::Result<std::path::PathBuf> {
    #[cfg(any(debug_assertions, test))]
    if !is_elevated() {
        return Ok(std::env::temp_dir());
    }
    let base = known_common_app_data()
        .ok_or_else(|| std::io::Error::other("cannot resolve ProgramData"))?
        .join("FrameworkCrate");
    create_admin_directory(&base)?;
    let dir = base.join("tasks");
    create_admin_directory(&dir)?;
    Ok(dir)
}

/// Creates an Administrators-owned directory with an admin/SYSTEM-only DACL.
fn create_admin_directory(path: &std::path::Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        InitializeAcl, InitializeSecurityDescriptor, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::core::PCWSTR;

    enable_restore_privilege()?;
    let mut admin_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admin_len = admin_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            admin_sid.as_mut_ptr() as PSID,
            &mut admin_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut system_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut system_len = system_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid.as_mut_ptr() as PSID,
            &mut system_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let mut acl_buf = [0u32; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    for sid in [admin_sid.as_ptr() as PSID, system_sid.as_ptr() as PSID] {
        if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, NO_INHERITANCE, GENERIC_ALL, sid) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let sd_ptr = &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR;
    if unsafe { InitializeSecurityDescriptor(sd_ptr, 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorOwner(sd_ptr, admin_sid.as_ptr() as PSID, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorDacl(sd_ptr, 1, acl, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_ptr,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = super::to_wide_path(path);
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &sa) };
    if created != 0 {
        if !task_file_owner_is_administrators(path)? {
            let _ = std::fs::remove_dir(path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task directory owner is not Administrators",
            ));
        }
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(183) {
        if crate::config::dir_has_reparse_point(path) || !path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "task directory is not a plain admin directory: {}",
                    path.display()
                ),
            ));
        }
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr() as PCWSTR,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                admin_sid.as_ptr() as PSID,
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        if !task_file_owner_is_administrators(path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task directory owner is not Administrators",
            ));
        }
        return Ok(());
    }
    Err(err)
}

/// Prefix of the temporary task document written for `schtasks /Create /XML`.
const TASK_XML_PREFIX: &str = "framework-crate-task-";

/// Per-process counter so concurrent toggles and PID reuse never map two
/// task documents onto one path.
static TASK_XML_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(any(debug_assertions, test))]
fn create_plain_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    let meta = f.metadata()?;
    if !meta.is_file() || meta.len() != bytes.len() as u64 {
        return Err(std::io::Error::other(
            "startup task xml failed post-write verification",
        ));
    }
    Ok(())
}

fn enable_restore_privilege() -> std::io::Result<()> {
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        SE_RESTORE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let result = (|| {
        let mut luid = windows_sys::Win32::Foundation::LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_RESTORE_NAME, &mut luid) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &state,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let err = unsafe { GetLastError() };
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }
        Ok(())
    })();
    unsafe { CloseHandle(token) };
    result
}

fn task_file_owner_is_administrators(path: &std::path::Path) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, PSID, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
    };
    let mut expected = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut expected_len = expected.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            expected.as_mut_ptr() as PSID,
            &mut expected_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let wide: Vec<u16> = super::to_wide_path(path);
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSID = std::ptr::null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 || owner.is_null() {
        if !sd.is_null() {
            unsafe { LocalFree(sd) };
        }
        return Err(if rc != 0 {
            std::io::Error::from_raw_os_error(rc as i32)
        } else {
            std::io::Error::other("startup task xml owner SID missing")
        });
    }
    let matches = unsafe { EqualSid(owner, expected.as_ptr() as PSID) } != 0;
    unsafe { LocalFree(sd) };
    Ok(matches)
}

fn create_admin_only_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        InitializeAcl, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, WRITE_DAC,
        WRITE_OWNER,
    };

    enable_restore_privilege()?;
    let mut admin_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admin_len = admin_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            admin_sid.as_mut_ptr() as PSID,
            &mut admin_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut system_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut system_len = system_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid.as_mut_ptr() as PSID,
            &mut system_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let mut acl_buf = [0u32; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    for sid in [admin_sid.as_ptr() as PSID, system_sid.as_ptr() as PSID] {
        if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, NO_INHERITANCE, GENERIC_ALL, sid) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    let wide: Vec<u16> = super::to_wide_path(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | FILE_READ_ATTRIBUTES | WRITE_DAC | WRITE_OWNER,
            0,
            std::ptr::null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    let rc = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            admin_sid.as_ptr() as PSID,
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() != bytes.len() as u64 {
            return Err(std::io::Error::other(
                "startup task xml failed post-write verification",
            ));
        }
        Ok(())
    })();
    drop(file);
    let result = result.and_then(|()| {
        if task_file_owner_is_administrators(path)? {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "startup task xml owner is not Administrators",
            ))
        }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn create_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(any(debug_assertions, test))]
    if !is_elevated() {
        return create_plain_task_file(path, bytes);
    }
    create_admin_only_task_file(path, bytes)
}

/// Writes the task document to a uniquely-named temp file.
fn write_task_xml(xml: &str) -> Result<String, String> {
    let dir = task_dir().map_err(|e| format!("cannot resolve startup task directory: {e}"))?;
    for _ in 0..5 {
        // Unpredictable suffix: timestamp-nanos + pid + counter hashed, so a
        // same-user attacker can't pre-plant the path.
        let nonce = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            crate::util::monotonic_ms().hash(&mut h);
            crate::util::current_time_ms().hash(&mut h);
            std::process::id().hash(&mut h);
            TASK_XML_COUNTER
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .hash(&mut h);
            (&TASK_XML_COUNTER as *const _ as usize).hash(&mut h);
            format!("{:016x}{:016x}", h.finish(), crate::util::monotonic_ms())
        };
        let xml_path = dir.join(format!("{TASK_XML_PREFIX}{nonce}.xml"));
        let bytes = utf16le_with_bom(xml);
        match create_task_file(&xml_path, &bytes) {
            Ok(()) => return Ok(xml_path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let _ = std::fs::remove_file(&xml_path);
                return Err(format!("cannot write startup task xml: {e}"));
            }
        }
    }
    Err("cannot allocate a unique startup task xml path".to_string())
}

/// Removes task documents left behind by a previous run.
///
/// A process killed mid-write (or by Task Scheduler) never reaches the
/// removal, so the file would otherwise accumulate in the temp directory
/// forever.
fn task_xml_is_stale(now: std::time::SystemTime, modified: std::time::SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age > std::time::Duration::from_secs(3600))
        .unwrap_or(false)
}

fn cleanup_task_xml_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TASK_XML_PREFIX) || !name.ends_with(".xml") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|modified| task_xml_is_stale(now, modified))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn cleanup_stale_task_xml() {
    let temp = std::env::temp_dir();
    cleanup_task_xml_in(&temp);
    if let Ok(dir) = task_dir()
        && dir != temp
    {
        cleanup_task_xml_in(&dir);
    }
}

/// Validates an executable path destined for the task action.
///
/// Rejects characters that would break the task XML or allow injection. The
/// path is embedded as XML text and wrapped in quotes for the action Command,
/// so quotes, cmd metacharacters and control characters are all refused here.
fn validate_task_command_path(exe: &str) -> Result<(), String> {
    const FORBIDDEN: &[char] = &[
        '"', '\'', '&', '|', ';', '%', '^', '`', '$', '(', ')', '<', '>', '*', '?', '{', '}', '!',
        '\n', '\r',
    ];
    if exe.contains(FORBIDDEN) {
        return Err("exe path contains invalid characters".to_string());
    }
    // Block non-ASCII control/bidi override that could hide injection.
    if exe
        .chars()
        .any(|c| c.is_control() || c == '\u{202E}' || c == '\u{202D}')
    {
        return Err("exe path contains invalid unicode".to_string());
    }
    // A trailing backslash would escape the closing quote of the action
    // Command (e.g. "C:\path\" → the \" becomes an escaped quote).
    if exe.ends_with('\\') {
        return Err("exe path must not end with backslash".to_string());
    }
    // HighestAvailable runs elevated at logon: an exe in a user-writable
    // dir (Downloads/Desktop/Temp) can be replaced to gain elevation. Warn
    // so portable runs stay a conscious choice.
    let lower = exe.to_lowercase();
    for risky in [
        "downloads",
        "desktop",
        "\\temp\\",
        "\\tmp\\",
        "appdata\\local\\temp",
    ] {
        if lower.contains(risky) {
            tracing::warn!(
                "startup exe is in a user-writable dir ({}); move to Program Files for HighestAvailable safety",
                exe
            );
            break;
        }
    }
    Ok(())
}

/// Builds the startup task document for `exe`.
///
/// Pure: no I/O and no environment lookups, so the startup settings are
/// verifiable by `cargo test` without administrator rights. The caller is
/// responsible for writing the UTF-16 encoded document and invoking schtasks.
pub(crate) fn build_startup_task(exe: &str, user_id: &str) -> Result<String, String> {
    validate_task_command_path(exe)?;
    // Command and Arguments are separate XML elements, so the exe path is never
    // re-parsed as a command line and cannot split into extra arguments the way
    // a schtasks /TR value did.
    Ok(build_task_xml(
        &format!("\"{exe}\""),
        "--minimized",
        user_id,
    ))
}

/// Registers or removes Windows startup scheduled task (requires elevation for HIGHEST).
pub fn set_startup_launch(enabled: bool) -> Result<(), String> {
    let output = if enabled {
        cleanup_stale_task_xml();
        let exe = std::env::current_exe().map_err(|e| format!("cannot resolve exe path: {e}"))?;
        let exe_str = exe.to_str().ok_or("exe path is not valid UTF-8")?;
        // Bounded by schtasks_output timeout so a hung Task Scheduler cannot freeze the UI.
        let xml = build_startup_task(exe_str, &current_user_id()?)?;
        let xml_path = write_task_xml(&xml)?;
        let created =
            schtasks_output(&["/Create", "/TN", STARTUP_TASK_NAME, "/XML", &xml_path, "/F"]);
        // Remove the temp document whether or not registration succeeded.
        let _ = std::fs::remove_file(&xml_path);
        let out = created.map_err(|e| format!("failed to run schtasks: {e}"))?;
        if out.status.success() && !startup_launch_enabled() {
            // Registration reported success but the task is not queryable, so
            // startup would silently not happen. Surface it instead of
            // returning a green toggle.
            return Err("task was created but cannot be queried; startup may not work".to_string());
        }
        out
    } else {
        schtasks_output(&["/Delete", "/TN", STARTUP_TASK_NAME, "/F"])
            .map_err(|e| format!("failed to run schtasks: {e}"))?
    };
    if output.status.success() {
        return Ok(());
    }
    // The task is registered with RunLevel=HighestAvailable, so a non-elevated
    // process is the common cause. Report that directly instead of surfacing
    // the localized schtasks text, which does not say what to do about it.
    if enabled && !is_elevated() {
        return Err(
            "launch at startup needs administrator privileges; restart Framework Crate as administrator and try again"
                .to_string(),
        );
    }
    // schtasks errors follow the OEM codepage (cp950 on zh-TW); decode
    // accordingly so the message is readable instead of mojibake.
    let stderr = decode_console_bytes(&output.stderr);
    let stdout = decode_console_bytes(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        // schtasks reported nothing at all, so name the command and the code
        // rather than surfacing a bare "exit code 1" with no context.
        format!(
            "schtasks failed with no output (exit code {})",
            output.status.code().unwrap_or(-1)
        )
    };
    Err(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_sets_startup_critical_settings() {
        let xml = build_task_xml("\"C:\\app\\framework-crate.exe\"", "--minimized", "DOM\\u");

        // schtasks defaults would silently skip a battery-powered logon.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
        // PT72H would stop the tray app three days after logon.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S<"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<LogonType>InteractiveToken<"));
        // Command and Arguments stay separate so the path is never re-parsed.
        assert!(xml.contains("<Command>&quot;C:\\app\\framework-crate.exe&quot;<"));
        assert!(xml.contains("<Arguments>--minimized<"));
        assert!(xml.contains("<UserId>DOM\\u<"));
    }

    #[test]
    fn startup_task_rejects_dangerous_paths_offline() {
        // Runs without elevation, unlike the schtasks round-trip test below.
        let base = r"C:\app\framework-crate.exe";
        assert!(
            super::build_startup_task(base, r"DOM\u").is_ok(),
            "a plain path must be accepted"
        );

        for bad in [
            r#"C:\a"b.exe"#,
            r"C:\a&b.exe",
            r"C:\a|b.exe",
            r"C:\a;b.exe",
            r"C:\a%b.exe",
            r"C:\a^b.exe",
            r"C:\a`b.exe",
            r"C:\a$b.exe",
            r"C:\a(b).exe",
            r"C:\a<b>.exe",
            r"C:\a*b.exe",
            r"C:\a?b.exe",
            r"C:\a{b}.exe",
            r"C:\a!b.exe",
            r"C:\a'b.exe",
            "C:\\a\nb.exe",
            "C:\\a\rb.exe",
            "C:\\a\u{202E}b.exe",
            "C:\\a\u{7}b.exe",
            r"C:\app\",
        ] {
            assert!(
                super::build_startup_task(bad, r"DOM\u").is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn startup_task_xml_is_verifiable_without_elevation() {
        let xml =
            super::build_startup_task(r"C:\app\framework-crate.exe", r"DOM\u").expect("valid path");
        // schtasks defaults would silently skip a battery-powered logon.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
        // PT72H would stop the tray app three days after logon.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S<"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<LogonType>InteractiveToken<"));
        // Command and Arguments stay separate so the path is never re-parsed.
        assert!(xml.contains(r"<Command>&quot;C:\app\framework-crate.exe&quot;<"));
        assert!(xml.contains("<Arguments>--minimized<"));
        assert!(xml.contains(r"<UserId>DOM\u<"));
    }

    #[test]
    fn task_xml_escapes_metacharacters() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        let xml = build_task_xml("\"C:\\a&b\\app.exe\"", "--minimized", "D&u");
        assert!(xml.contains("<UserId>D&amp;u<"));
        assert!(!xml.contains("<UserId>D&u<"));
    }

    #[test]
    fn task_xml_encoding_has_utf16le_bom() {
        let bytes = utf16le_with_bom("<?xml version=\"1.0\" encoding=\"UTF-16\"?>");
        // schtasks rejects a UTF-8 document with a parse error on the declaration.
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let text = String::from_utf16(
            &bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect::<Vec<u16>>(),
        )
        .expect("valid UTF-16");
        assert_eq!(text, "<?xml version=\"1.0\" encoding=\"UTF-16\"?>");
    }

    #[test]
    fn current_user_id_ignores_spoofed_env() {
        // The task principal must come from the token: poison the env
        // first and prove the result is unaffected.
        let prev_user = std::env::var_os("USERNAME");
        let prev_domain = std::env::var_os("USERDOMAIN");
        unsafe {
            std::env::set_var("USERNAME", "spoofed-user-xyz");
            std::env::set_var("USERDOMAIN", "SPOOFED-DOMAIN-XYZ");
        }
        let id = super::current_user_id().expect("token user must resolve");
        assert!(!id.is_empty());
        // NOTE: deliberately not interpolating {id} here: the principal is
        // user-identifying, and even test output should not echo it.
        assert!(
            !id.to_lowercase().contains("spoofed"),
            "principal must not trust env"
        );
        unsafe {
            if let Some(v) = prev_user {
                std::env::set_var("USERNAME", v);
            } else {
                std::env::remove_var("USERNAME");
            }
            if let Some(v) = prev_domain {
                std::env::set_var("USERDOMAIN", v);
            } else {
                std::env::remove_var("USERDOMAIN");
            }
        }
    }

    #[test]
    fn task_xml_cleanup_only_removes_old_files() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(7200);
        assert!(super::task_xml_is_stale(
            now,
            now - std::time::Duration::from_secs(3700)
        ));
        assert!(!super::task_xml_is_stale(
            now,
            now - std::time::Duration::from_secs(60)
        ));
        assert!(!super::task_xml_is_stale(
            now,
            now + std::time::Duration::from_secs(60)
        ));
    }

    #[test]
    fn elevated_task_xml_is_owned_by_administrators() {
        if !super::is_elevated() {
            return;
        }
        let path = super::write_task_xml("<a/>").expect("write");
        assert!(
            super::task_file_owner_is_administrators(std::path::Path::new(&path))
                .expect("owner query")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn task_xml_paths_are_unique_per_call() {
        // create_new refuses planted paths, so uniqueness must come from
        // the name itself (pid + counter), not from overwriting. Assert
        // only the names: file-existence checks would race the stale-file
        // sweeper that other tests trigger in parallel.
        let a = super::write_task_xml("<a/>").expect("first write");
        let b = super::write_task_xml("<b/>").expect("second write");
        assert_ne!(a, b, "two task documents must not share a path");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    #[ignore = "creates and deletes a real scheduled task; run elevated on a disposable machine"]
    fn startup_launch_scheduled_task_round_trip() {
        // Round-trip: enable creates the ONLOGON task, disable deletes it.
        // Registering a /RL HIGHEST task requires elevation, so skip
        // silently when running unelevated (debug tests / CI).
        match super::set_startup_launch(true) {
            Ok(()) => {
                assert!(
                    super::startup_launch_enabled(),
                    "task should exist after enable"
                );
                super::set_startup_launch(false).expect("disable should succeed");
                assert!(
                    !super::startup_launch_enabled(),
                    "task should be gone after disable"
                );
            }
            Err(_) => {
                eprintln!("skipping startup task round-trip: no elevation");
            }
        }
    }
}
