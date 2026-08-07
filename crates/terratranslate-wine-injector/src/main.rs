use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(about = "Inject the TerraTranslate hook bridge into a Wine process")]
struct Arguments {
    #[arg(long)]
    process_id: u32,
    #[arg(long)]
    dll: PathBuf,
}

#[cfg(not(windows))]
fn main() {
    let _ = Arguments::parse();
    eprintln!(
        "terratranslate-wine-injector must be cross-compiled for Windows and run inside the target Wine prefix"
    );
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    let arguments = Arguments::parse();
    if let Err(error) = windows_impl::inject(arguments.process_id, &arguments.dll) {
        eprintln!("injection failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::{CString, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
    };
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, INFINITE, OpenProcess, PROCESS_CREATE_THREAD,
        PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
        WaitForSingleObject,
    };

    pub fn inject(process_id: u32, dll: &Path) -> Result<(), String> {
        let canonical = dll.canonicalize().map_err(|error| error.to_string())?;
        let mut path: Vec<u16> = OsStr::new(&canonical)
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            let process = OpenProcess(
                PROCESS_CREATE_THREAD
                    | PROCESS_QUERY_INFORMATION
                    | PROCESS_VM_OPERATION
                    | PROCESS_VM_WRITE
                    | PROCESS_VM_READ,
                0,
                process_id,
            );
            if process.is_null() {
                return Err(last_error("OpenProcess"));
            }
            let remote = VirtualAllocEx(
                process,
                ptr::null(),
                path.len() * std::mem::size_of::<u16>(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if remote.is_null() {
                CloseHandle(process);
                return Err(last_error("VirtualAllocEx"));
            }
            let written = WriteProcessMemory(
                process,
                remote,
                path.as_ptr().cast(),
                path.len() * std::mem::size_of::<u16>(),
                ptr::null_mut(),
            );
            if written == 0 {
                VirtualFreeEx(process, remote, 0, MEM_RELEASE);
                CloseHandle(process);
                return Err(last_error("WriteProcessMemory"));
            }
            let kernel_name: Vec<u16> = OsStr::new("kernel32.dll")
                .encode_wide()
                .chain(Some(0))
                .collect();
            let kernel = GetModuleHandleW(kernel_name.as_ptr());
            let load_library = GetProcAddress(
                kernel,
                CString::new("LoadLibraryW")
                    .unwrap()
                    .as_c_str()
                    .as_ptr()
                    .cast(),
            );
            let start = std::mem::transmute(load_library);
            let thread =
                CreateRemoteThread(process, ptr::null(), 0, start, remote, 0, ptr::null_mut());
            if thread.is_null() {
                VirtualFreeEx(process, remote, 0, MEM_RELEASE);
                CloseHandle(process);
                return Err(last_error("CreateRemoteThread"));
            }
            WaitForSingleObject(thread, INFINITE);
            CloseHandle(thread);
            VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            CloseHandle(process);
        }
        Ok(())
    }

    unsafe fn last_error(operation: &str) -> String {
        format!("{operation} returned Windows error {}", unsafe {
            GetLastError()
        })
    }
}
