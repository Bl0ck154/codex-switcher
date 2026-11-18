//! Small, synchronous Windows process snapshot. No shells, WMI providers or persistent handles.
//! The caller runs this on Tauri's blocking pool, never on the async executor.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;

use anyhow::{bail, Context};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

use super::WindowsCodexProcess;

const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60; // Windows 8.1+
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const MAX_COMMAND_LINE_BYTES: usize = 128 * 1024;
const ERROR_INVALID_PARAMETER: i32 = 87;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        info_class: i32,
        buffer: *mut c_void,
        buffer_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn utf16_z(units: &[u16]) -> String {
    let len = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len])
}

fn candidates() -> anyhow::Result<Vec<(u32, u32, String)>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("Toolhelp process snapshot failed");
    }
    let _snapshot = OwnedHandle(snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return Err(io::Error::last_os_error()).context("Toolhelp process enumeration failed");
    }
    loop {
        let name = utf16_z(&entry.szExeFile);
        if name.eq_ignore_ascii_case("codex.exe")
            || name.eq_ignore_ascii_case("chatgpt.exe")
        {
            found.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
        }
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    Ok(found)
}

unsafe extern "system" fn collect_window_title(hwnd: HWND, context: LPARAM) -> i32 {
    let titles = &mut *(context as *mut HashMap<u32, String>);
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return 1;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 || titles.contains_key(&pid) {
        return 1;
    }
    let mut title = vec![0u16; length as usize + 1];
    let read = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
    if read > 0 {
        titles.insert(pid, String::from_utf16_lossy(&title[..read as usize]));
    }
    1
}

fn window_titles() -> anyhow::Result<HashMap<u32, String>> {
    let mut titles = HashMap::new();
    let ok = unsafe {
        EnumWindows(
            Some(collect_window_title),
            (&mut titles as *mut HashMap<u32, String>) as LPARAM,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("EnumWindows failed");
    }
    Ok(titles)
}

fn open_query_process(pid: u32) -> anyhow::Result<Option<OwnedHandle>> {
    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS, 0, pid)
    };
    if handle.is_null() {
        let err = io::Error::last_os_error();
        // A process can exit between Toolhelp enumeration and OpenProcess.
        return if err.raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
            Ok(None)
        } else {
            Err(err).with_context(|| format!("Cannot inspect Codex candidate PID {pid}"))
        };
    }
    Ok(Some(OwnedHandle(handle)))
}

fn executable_path(handle: HANDLE) -> anyhow::Result<String> {
    let mut path = vec![0u16; 32768];
    let mut len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error()).context("QueryFullProcessImageNameW failed");
    }
    Ok(String::from_utf16_lossy(&path[..len as usize]))
}

/// ProcessCommandLineInformation returns a UNICODE_STRING whose pointer is in the
/// caller-owned response buffer. Verify its bounds before dereferencing it.
/// A failure is an error, not an empty command line: ignoring inaccessible
/// renderer flags could make account switching unsafe.
fn command_line(handle: HANDLE) -> anyhow::Result<String> {
    let mut requested = 2048usize;
    loop {
        if requested > MAX_COMMAND_LINE_BYTES {
            bail!("Codex command line exceeded the safety bound");
        }
        // usize storage provides pointer alignment for UNICODE_STRING.
        let words = requested.div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; words];
        let capacity = storage.len() * size_of::<usize>();
        let mut required = 0u32;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                storage.as_mut_ptr().cast(),
                capacity as u32,
                &mut required,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            requested = (required as usize).max(capacity * 2);
            continue;
        }
        if status < 0 {
            bail!("NtQueryInformationProcess(CommandLine) failed: NTSTATUS {status:#x}");
        }
        let response = unsafe { &*(storage.as_ptr() as *const UnicodeString) };
        let len = response.length as usize;
        let base = storage.as_ptr() as usize;
        let start = response.buffer as usize;
        let end = start.checked_add(len).context("Command line length overflow")?;
        if len % 2 != 0
            || len > response.maximum_length as usize
            || start < base
            || end > base + capacity
        {
            bail!("Invalid Windows command line buffer");
        }
        let units = unsafe { std::slice::from_raw_parts(response.buffer, len / 2) };
        return Ok(String::from_utf16_lossy(units));
    }
}

/// Fresh, independent snapshot for every safety-critical operation. Never cached.
pub(super) fn read() -> anyhow::Result<Vec<WindowsCodexProcess>> {
    let candidates = candidates()?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let titles = window_titles()?;
    let mut result = Vec::with_capacity(candidates.len());
    for (pid, parent, name) in candidates {
        let Some(handle) = open_query_process(pid)? else {
            continue;
        };
        // A handle also prevents PID reuse while inspecting this candidate.
        if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_TIMEOUT {
            continue;
        }
        let path = match executable_path(handle.0) {
            Ok(path) => path,
            Err(_error) if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_TIMEOUT => {
                continue; // Exited while inspecting.
            }
            Err(error) => return Err(error),
        };
        if !path.rsplit(['\\', '/']).next().is_some_and(|actual| actual.eq_ignore_ascii_case(&name)) {
            continue; // PID reused for a different executable.
        }
        let command = match command_line(handle.0) {
            Ok(command) => command,
            Err(_error) if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_TIMEOUT => {
                continue; // Exited while querying the command line.
            }
            Err(error) => return Err(error),
        };
        if command.is_empty() {
            bail!("Missing command line for candidate PID {pid}");
        }
        result.push(WindowsCodexProcess {
            name,
            process_id: pid,
            parent_process_id: parent,
            command_line: command,
            executable_path: path,
            main_window_title: titles.get(&pid).cloned().unwrap_or_default(),
        });
    }
    Ok(result)
}

pub(super) fn command_line_for_pid(pid: u32) -> Option<String> {
    let handle = open_query_process(pid).ok()??;
    if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_TIMEOUT {
        return None;
    }
    command_line(handle.0).ok()
}

pub(super) fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(SYNCHRONIZE_ACCESS, 0, pid) };
    if handle.is_null() {
        // An inaccessible process is not proof that it has exited.
        return io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER);
    }
    let handle = OwnedHandle(handle);
    unsafe { WaitForSingleObject(handle.0, 0) } != 0 // WAIT_OBJECT_0 = 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolhelp_enumeration_succeeds_without_a_shell() {
        let entries = candidates().expect("native Windows snapshot must be available");
        assert!(entries.iter().all(|(pid, _, _)| *pid > 0));
    }

    #[test]
    fn current_process_liveness_is_observable() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(0));
    }

    #[test]
    fn own_command_line_is_readable_without_powershell() {
        assert!(!command_line_for_pid(std::process::id())
            .expect("own process command line")
            .is_empty());
    }
}
