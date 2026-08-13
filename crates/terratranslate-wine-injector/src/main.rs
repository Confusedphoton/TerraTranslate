use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(about = "List Wine processes or inject the TerraTranslate hook bridge")]
struct Arguments {
    /// List processes visible in the current Wine prefix as JSON lines.
    #[arg(long)]
    list: bool,
    #[arg(long, required_unless_present = "list")]
    process_id: Option<u32>,
    #[arg(long, required_unless_present = "list")]
    dll: Option<PathBuf>,
    /// Guest-visible path to the authenticated bridge configuration.
    #[arg(long, required_unless_present = "list")]
    config: Option<PathBuf>,
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u32,
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
    let result = if arguments.list {
        windows_impl::list_processes()
    } else {
        windows_impl::inject(
            arguments.process_id.expect("clap requires process id"),
            arguments.dll.as_deref().expect("clap requires DLL"),
            arguments.config.as_deref().expect("clap requires config"),
            arguments.timeout_ms,
        )
    };
    if let Err(error) = result {
        eprintln!("wine hook operation failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::{CString, OsStr, c_void};
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use serde_json::json;
    use terratranslate_wine_injector::{PeArchitecture, read_pe_architecture};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
        Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        DONT_RESOLVE_DLL_REFERENCES, FreeLibrary, GetModuleHandleW, GetProcAddress, LoadLibraryExW,
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
    };
    use windows_sys::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
        IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
    };
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, GetCurrentProcess, GetExitCodeThread, IsWow64Process2, OpenProcess,
        PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, QueryFullProcessImageNameW,
        WaitForSingleObject,
    };

    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;

    pub fn inject(
        process_id: u32,
        dll: &Path,
        config: &Path,
        timeout_ms: u32,
    ) -> Result<(), String> {
        let dll = dll
            .canonicalize()
            .map_err(|error| format!("resolve DLL {}: {error}", dll.display()))?;
        let config = config
            .canonicalize()
            .map_err(|error| format!("resolve config {}: {error}", config.display()))?;
        let dll_architecture = read_pe_architecture(&dll)?;

        unsafe {
            let process = OwnedHandle::new(OpenProcess(
                PROCESS_CREATE_THREAD
                    | PROCESS_QUERY_INFORMATION
                    | PROCESS_VM_OPERATION
                    | PROCESS_VM_WRITE
                    | PROCESS_VM_READ,
                0,
                process_id,
            ))
            .ok_or_else(|| last_error("OpenProcess"))?;

            let target_machine = process_machine(process.0)?;
            let injector_machine = process_machine(GetCurrentProcess())?;
            if target_machine != injector_machine {
                return Err(format!(
                    "injector architecture {} does not match target {}; run the matching injector",
                    machine_name(injector_machine),
                    machine_name(target_machine)
                ));
            }
            if pe_machine(dll_architecture) != target_machine {
                return Err(format!(
                    "DLL architecture {dll_architecture} does not match target {}",
                    machine_name(target_machine)
                ));
            }

            let dll_argument = RemoteWideString::new(process.0, &dll)?;
            let kernel = module_handle("kernel32.dll")?;
            let load_library = GetProcAddress(kernel, c"LoadLibraryW".as_ptr().cast())
                .ok_or_else(|| last_error("GetProcAddress(LoadLibraryW)"))?;
            let load_thread = remote_thread(
                process.0,
                std::mem::transmute(load_library),
                dll_argument.pointer,
            )?;
            wait_thread(load_thread.0, timeout_ms, "LoadLibraryW")?;
            let mut load_status = 0;
            if GetExitCodeThread(load_thread.0, &mut load_status) == 0 || load_status == 0 {
                return Err(last_error("remote LoadLibraryW"));
            }
            drop(load_thread);
            drop(dll_argument);

            let remote_module = find_remote_module(process_id, &dll)?;
            let startup_offset = exported_offset(&dll, "TerraTranslateHookStartW")?;
            let remote_startup = (remote_module as usize + startup_offset) as *const c_void;
            let config_argument = RemoteWideString::new(process.0, &config)?;
            let startup_thread = remote_thread(
                process.0,
                Some(std::mem::transmute(remote_startup)),
                config_argument.pointer,
            )?;
            wait_thread(startup_thread.0, timeout_ms, "TerraTranslateHookStartW")?;
            let mut startup_status = 0;
            if GetExitCodeThread(startup_thread.0, &mut startup_status) == 0 {
                return Err(last_error("GetExitCodeThread(startup)"));
            }
            if startup_status != 1 {
                return Err(format!(
                    "TerraTranslateHookStartW returned failure status {startup_status}"
                ));
            }
        }
        Ok(())
    }

    pub fn list_processes() -> Result<(), String> {
        unsafe {
            let snapshot = OwnedHandle::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0))
                .ok_or_else(|| last_error("CreateToolhelp32Snapshot(processes)"))?;
            let mut entry: PROCESSENTRY32W = zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snapshot.0, &mut entry) == 0 {
                return Err(last_error("Process32FirstW"));
            }
            loop {
                if let Some(process) = OwnedHandle::new(OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ProcessID,
                )) {
                    let mut path = vec![0_u16; 32 * 1024];
                    let mut length = path.len() as u32;
                    if QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut length) != 0
                    {
                        path.truncate(length as usize);
                        let executable = String::from_utf16_lossy(&path);
                        if executable.to_ascii_lowercase().ends_with(".exe") {
                            let architecture = process_machine(process.0)
                                .map(machine_name)
                                .unwrap_or("unknown");
                            println!(
                                "{}",
                                json!({
                                    "process_id": entry.th32ProcessID,
                                    "executable": executable,
                                    "architecture": architecture,
                                    "wine_prefix": std::env::var("WINEPREFIX").ok(),
                                })
                            );
                        }
                    }
                }
                if Process32NextW(snapshot.0, &mut entry) == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    unsafe fn process_machine(process: HANDLE) -> Result<IMAGE_FILE_MACHINE, String> {
        let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
        let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
        if unsafe { IsWow64Process2(process, &mut process_machine, &mut native_machine) } == 0 {
            return Err(last_error("IsWow64Process2"));
        }
        Ok(if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
            native_machine
        } else {
            process_machine
        })
    }

    fn pe_machine(architecture: PeArchitecture) -> IMAGE_FILE_MACHINE {
        match architecture {
            PeArchitecture::X86 => IMAGE_FILE_MACHINE_I386,
            PeArchitecture::X86_64 => IMAGE_FILE_MACHINE_AMD64,
            PeArchitecture::Arm64 => IMAGE_FILE_MACHINE_ARM64,
            PeArchitecture::Other(machine) => machine,
        }
    }

    fn machine_name(machine: IMAGE_FILE_MACHINE) -> &'static str {
        match machine {
            IMAGE_FILE_MACHINE_I386 => "x86",
            IMAGE_FILE_MACHINE_AMD64 => "x86_64",
            IMAGE_FILE_MACHINE_ARM64 => "arm64",
            _ => "unsupported",
        }
    }

    unsafe fn remote_thread(
        process: HANDLE,
        start: windows_sys::Win32::System::Threading::LPTHREAD_START_ROUTINE,
        argument: *mut c_void,
    ) -> Result<OwnedHandle, String> {
        OwnedHandle::new(unsafe {
            CreateRemoteThread(process, ptr::null(), 0, start, argument, 0, ptr::null_mut())
        })
        .ok_or_else(|| last_error("CreateRemoteThread"))
    }

    unsafe fn wait_thread(thread: HANDLE, timeout_ms: u32, operation: &str) -> Result<(), String> {
        match unsafe { WaitForSingleObject(thread, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(format!("{operation} timed out after {timeout_ms} ms")),
            _ => Err(last_error(&format!("WaitForSingleObject({operation})"))),
        }
    }

    unsafe fn find_remote_module(process_id: u32, dll: &Path) -> Result<*mut c_void, String> {
        let expected = dll
            .file_name()
            .ok_or_else(|| "DLL path has no file name".to_owned())?
            .to_string_lossy()
            .to_ascii_lowercase();
        let snapshot = OwnedHandle::new(unsafe {
            CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, process_id)
        })
        .ok_or_else(|| last_error("CreateToolhelp32Snapshot(modules)"))?;
        let mut entry: MODULEENTRY32W = unsafe { zeroed() };
        entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
        if unsafe { Module32FirstW(snapshot.0, &mut entry) } == 0 {
            return Err(last_error("Module32FirstW"));
        }
        loop {
            let length = entry
                .szModule
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szModule.len());
            if String::from_utf16_lossy(&entry.szModule[..length]).to_ascii_lowercase() == expected
            {
                return Ok(entry.modBaseAddr.cast());
            }
            if unsafe { Module32NextW(snapshot.0, &mut entry) } == 0 {
                break;
            }
        }
        Err(format!(
            "loaded DLL {expected} was not found in target module list"
        ))
    }

    unsafe fn exported_offset(dll: &Path, export: &str) -> Result<usize, String> {
        let wide = wide(dll.as_os_str());
        let local =
            unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES) };
        if local.is_null() {
            return Err(last_error("LoadLibraryExW(local image)"));
        }
        let export_name = CString::new(export).map_err(|error| error.to_string())?;
        let address = unsafe { GetProcAddress(local, export_name.as_ptr().cast()) };
        let result = address
            .map(|address| address as usize - local as usize)
            .ok_or_else(|| last_error("GetProcAddress(TerraTranslateHookStartW)"));
        unsafe { FreeLibrary(local) };
        result
    }

    unsafe fn module_handle(name: &str) -> Result<*mut c_void, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if handle.is_null() {
            Err(last_error("GetModuleHandleW"))
        } else {
            Ok(handle)
        }
    }

    struct RemoteWideString {
        process: HANDLE,
        pointer: *mut c_void,
    }

    impl RemoteWideString {
        unsafe fn new(process: HANDLE, path: &Path) -> Result<Self, String> {
            let value = wide(path.as_os_str());
            let size = value.len() * size_of::<u16>();
            let pointer = unsafe {
                VirtualAllocEx(
                    process,
                    ptr::null(),
                    size,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if pointer.is_null() {
                return Err(last_error("VirtualAllocEx"));
            }
            if unsafe {
                WriteProcessMemory(
                    process,
                    pointer,
                    value.as_ptr().cast(),
                    size,
                    ptr::null_mut(),
                )
            } == 0
            {
                unsafe { VirtualFreeEx(process, pointer, 0, MEM_RELEASE) };
                return Err(last_error("WriteProcessMemory"));
            }
            Ok(Self { process, pointer })
        }
    }

    impl Drop for RemoteWideString {
        fn drop(&mut self) {
            unsafe { VirtualFreeEx(self.process, self.pointer, 0, MEM_RELEASE) };
        }
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> Option<Self> {
            (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn last_error(operation: &str) -> String {
        format!("{operation} returned Windows error {}", unsafe {
            GetLastError()
        })
    }
}
