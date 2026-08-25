use std::cell::Cell;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use minhook::MinHook;
use terratranslate_wine_protocol::{
    BridgeHello, BridgeMessage, ExecutableIdentity, HookBridgeConfig, HookPlatform, HookRuntime,
    HostMessage, MAX_IDENTITY_BYTES, PROTOCOL_VERSION, ProcessArchitecture, decode, encode,
};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{FILETIME, HINSTANCE, TRUE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::core::BOOL;

use crate::{CandidateBook, Observation, bounded_utf8};

mod directwrite;
mod gdi;
mod harfbuzz;
mod uniscribe;

const DLL_PROCESS_ATTACH: u32 = 1;
const EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_WIRE_MESSAGE: usize = 1024 * 1024;

static OBSERVATIONS: OnceLock<Mutex<Option<SyncSender<Observation>>>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);
static DROPPED_OBSERVATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static INSIDE_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// The loader-lock-safe entry point. All initialization is deferred until the
/// injector calls `TerraTranslateHookStartW` after `LoadLibraryW` returns.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    module: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: `module` is the handle supplied by the loader for this DLL.
        unsafe { DisableThreadLibraryCalls(module) };
    }
    TRUE
}

/// Starts IPC and installs hooks on a Rust-owned worker thread.
///
/// Returns immediately. A nonzero result means the worker was created or was
/// already running; initialization diagnostics are sent over IPC.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn TerraTranslateHookStartW(config_path: *const u16) -> u32 {
    if config_path.is_null() {
        return 0;
    }
    // Loading an already attached DLL is a normal outcome when the user
    // refreshes the target list, retries an attach, or reconnects after the
    // host application restarted. Keep the operation idempotent instead of
    // making the injector report a false failure.
    if STARTED.swap(true, Ordering::AcqRel) {
        return 1;
    }
    let Some(path) = (unsafe { copy_nul_terminated(config_path, 32 * 1024) }) else {
        STARTED.store(false, Ordering::Release);
        return 0;
    };
    let Ok(_) = std::thread::Builder::new()
        .name("terratranslate-hook".into())
        .spawn(move || worker_main(PathBuf::from(path)))
    else {
        STARTED.store(false, Ordering::Release);
        return 0;
    };
    1
}

fn worker_main(config_path: PathBuf) {
    if read_config(&config_path).is_none() {
        reset_worker_state();
        return;
    }
    let (sender, receiver) = sync_channel(EVENT_QUEUE_CAPACITY);
    if let Ok(mut observations) = observation_slot().lock() {
        *observations = Some(sender);
    } else {
        reset_worker_state();
        return;
    }

    if !install_hooks() {
        reset_worker_state();
        return;
    }
    ACTIVE.store(true, Ordering::Release);

    let executable = bounded_utf8(
        std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        MAX_IDENTITY_BYTES,
    );
    let executable = ExecutableIdentity {
        path: executable,
        image_id: None,
    };
    let mut book = CandidateBook::new(executable.clone());
    while ACTIVE.load(Ordering::Acquire) {
        let Some((config, authentication_token)) = read_config(&config_path) else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        match BridgeSocket::connect(&config) {
            Ok(mut stream) => {
                let hello = BridgeMessage::Hello(BridgeHello {
                    protocol_version: PROTOCOL_VERSION,
                    authentication_token,
                    bridge_id: Uuid::new_v4(),
                    platform: HookPlatform::Windows,
                    runtime: HookRuntime::Wine,
                    process_id: unsafe { GetCurrentProcessId() },
                    architecture: if usize::BITS == 32 {
                        ProcessArchitecture::X86
                    } else {
                        ProcessArchitecture::X86_64
                    },
                    executable: executable.clone(),
                    adapters: vec![
                        "gdi".into(),
                        "uniscribe".into(),
                        "directwrite".into(),
                        "harfbuzz".into(),
                    ],
                });
                if stream.write_message(&hello).is_ok() {
                    book.reset_connection();
                    for advertisement in book.advertisements() {
                        if stream.write_message(&advertisement).is_err() {
                            break;
                        }
                    }
                    service_connection(&mut stream, &receiver, &mut book);
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    book.disable_all();
    reset_worker_state();
}

fn read_config(config_path: &Path) -> Option<(HookBridgeConfig, [u8; 32])> {
    let bytes = std::fs::read(config_path).ok()?;
    let config = serde_json::from_slice::<HookBridgeConfig>(&bytes).ok()?;
    let authentication_token = config.authentication_token().ok()?;
    Some((config, authentication_token))
}

fn observation_slot() -> &'static Mutex<Option<SyncSender<Observation>>> {
    OBSERVATIONS.get_or_init(|| Mutex::new(None))
}

fn install_hooks() -> bool {
    let hooks = std::panic::catch_unwind(|| unsafe {
        if !HOOKS_INSTALLED.load(Ordering::Acquire) {
            gdi::install();
            uniscribe::install();
            directwrite::install();
            harfbuzz::install();
        }
        MinHook::enable_all_hooks()
    });
    if matches!(hooks, Ok(Ok(()))) {
        HOOKS_INSTALLED.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn reset_worker_state() {
    ACTIVE.store(false, Ordering::Release);
    if let Ok(mut observations) = observation_slot().lock() {
        *observations = None;
    }
    if HOOKS_INSTALLED.load(Ordering::Acquire) {
        let _ = std::panic::catch_unwind(|| unsafe { MinHook::disable_all_hooks() });
    }
    STARTED.store(false, Ordering::Release);
}

fn service_connection(
    stream: &mut BridgeSocket,
    receiver: &Receiver<Observation>,
    book: &mut CandidateBook,
) {
    let mut last_diagnostic = Instant::now();
    while ACTIVE.load(Ordering::Acquire) {
        loop {
            match stream.read_message::<HostMessage>() {
                Ok(Some(HostMessage::EnableCandidate(id))) => book.set_enabled(id, true),
                Ok(Some(HostMessage::DisableCandidate(id))) => book.set_enabled(id, false),
                Ok(Some(HostMessage::Ping(value))) => {
                    let _ = stream.write_message(&BridgeMessage::Pong(value));
                }
                Ok(Some(HostMessage::Shutdown)) => {
                    book.disable_all();
                    ACTIVE.store(false, Ordering::Release);
                    return;
                }
                Ok(Some(HostMessage::Reject { .. })) | Err(_) => {
                    book.disable_all();
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
            }
        }

        match receiver.try_recv() {
            Ok(observation) => {
                for message in book.observe(observation) {
                    if stream.write_message(&message).is_err() {
                        book.disable_all();
                        return;
                    }
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            Err(TryRecvError::Disconnected) => return,
        }
        if last_diagnostic.elapsed() >= Duration::from_secs(5) {
            let dropped = DROPPED_OBSERVATIONS.swap(0, Ordering::Relaxed);
            if dropped > 0
                && stream
                    .write_message(&BridgeMessage::Diagnostic {
                        level: "warning".into(),
                        message: format!(
                            "dropped {dropped} text observations while the hook queue was full"
                        ),
                    })
                    .is_err()
            {
                book.disable_all();
                return;
            }
            last_diagnostic = Instant::now();
        }
    }
}

pub(super) fn observe(adapter: &'static str, api: &'static str, text: String) {
    if !ACTIVE.load(Ordering::Relaxed) || text.is_empty() {
        return;
    }
    INSIDE_HOOK.with(|inside| {
        if inside.replace(true) {
            return;
        }
        let _reset = RecursionReset(inside);
        let (callsite_module, callsite_offset) = callsite();
        let observation = Observation {
            adapter,
            api,
            callsite_module,
            callsite_offset,
            text,
            thread_id: unsafe { GetCurrentThreadId() },
            timestamp_ms: unix_timestamp_ms(),
        };
        let sender = observation_slot()
            .lock()
            .ok()
            .and_then(|observations| observations.as_ref().cloned());
        if let Some(sender) = sender {
            if sender.try_send(observation).is_err() {
                DROPPED_OBSERVATIONS.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}

fn unix_timestamp_ms() -> i64 {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    let mut time = FILETIME::default();
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    let ticks = ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
    ticks.saturating_sub(WINDOWS_TO_UNIX_EPOCH_100NS) as i64 / 10_000
}

struct RecursionReset<'a>(&'a Cell<bool>);

impl Drop for RecursionReset<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn callsite() -> (String, u64) {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

    let mut frames = [std::ptr::null_mut(); 8];
    let count = unsafe {
        RtlCaptureStackBackTrace(
            2,
            frames.len() as u32,
            frames.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    for address in frames.into_iter().take(count as usize) {
        let mut memory = MEMORY_BASIC_INFORMATION::default();
        if unsafe {
            VirtualQuery(
                address,
                &mut memory,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        } == 0
        {
            continue;
        }
        let module = memory.AllocationBase as HINSTANCE;
        let mut name = vec![0_u16; 1024];
        let length = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) };
        if length == 0 {
            continue;
        }
        name.truncate(length as usize);
        let path = String::from_utf16_lossy(&name).to_ascii_lowercase();
        if path.contains("terratranslate_wine_hook") || path.contains("terratranslate-wine-hook") {
            continue;
        }
        return (path, address as usize as u64 - module as usize as u64);
    }
    ("unknown".into(), 0)
}

unsafe fn copy_nul_terminated(pointer: *const u16, maximum: usize) -> Option<String> {
    let mut length = 0;
    while length < maximum && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    if length == maximum {
        return None;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(pointer, length)
    }))
}

struct BridgeSocket {
    stream: TcpStream,
    input: Vec<u8>,
}

impl BridgeSocket {
    fn connect(config: &HookBridgeConfig) -> Result<Self, ()> {
        let port = config.tcp_port.ok_or(())?;
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|_| ())?;
        stream.set_nonblocking(true).map_err(|_| ())?;
        Ok(Self {
            stream,
            input: Vec::with_capacity(4096),
        })
    }

    fn write_message<T: serde::Serialize>(&mut self, message: &T) -> Result<(), ()> {
        let payload = encode(message).map_err(|_| ())?;
        if payload.len() > MAX_WIRE_MESSAGE {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(payload.len() + 4);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        let mut sent = 0;
        while sent < bytes.len() {
            match self.stream.write(&bytes[sent..]) {
                Ok(0) => return Err(()),
                Ok(written) => sent += written,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return Err(()),
            }
        }
        Ok(())
    }

    fn read_message<T: for<'de> serde::Deserialize<'de>>(&mut self) -> Result<Option<T>, ()> {
        let mut buffer = [0_u8; 4096];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(()),
                Ok(read) => {
                    self.input.extend_from_slice(&buffer[..read]);
                    if self.input.len() > MAX_WIRE_MESSAGE + 4 {
                        return Err(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(()),
            }
        }
        if self.input.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_le_bytes(self.input[..4].try_into().unwrap()) as usize;
        if length > MAX_WIRE_MESSAGE {
            return Err(());
        }
        if self.input.len() < length + 4 {
            return Ok(None);
        }
        let message = decode(&self.input[4..length + 4], MAX_WIRE_MESSAGE).map_err(|_| ())?;
        self.input.drain(..length + 4);
        Ok(Some(message))
    }
}
