//! Semantic Linux text interception loaded into a target process with `LD_PRELOAD`.
//!
//! The hot path never opens a socket or waits on a full queue. It calls the original rendering
//! function exactly once and offers a bounded text copy to a lazy background sender.

#![allow(clippy::missing_safety_doc)]

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use terratranslate_wine_protocol::{
    BridgeHello, BridgeMessage, ExecutableIdentity, HookBridgeConfig, HookCandidate, HookPlatform,
    HookRuntime, HookTextEvent, HostMessage, MAX_IDENTITY_BYTES, MAX_SAMPLE_BYTES, MAX_TEXT_BYTES,
    MAX_WIRE_MESSAGE_BYTES, PROTOCOL_VERSION, ProcessArchitecture, StableCandidateKey, decode,
    encode,
};
use uuid::Uuid;

const OBSERVATION_QUEUE: usize = 256;
const MAX_CANDIDATES: usize = 1024;
const MAX_PENDING_EVENTS: usize = 256;
const DEDUP_WINDOW: Duration = Duration::from_millis(100);
const CONNECT_RETRY: Duration = Duration::from_secs(1);

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adapter {
    Pango,
    SdlTtf,
    Cairo,
}

impl Adapter {
    fn id(self) -> &'static str {
        match self {
            Self::Pango => "pango",
            Self::SdlTtf => "sdl_ttf",
            Self::Cairo => "cairo",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Pango => 3,
            Self::SdlTtf => 2,
            Self::Cairo => 1,
        }
    }
}

#[derive(Debug)]
struct Observation {
    adapter: Adapter,
    api: &'static str,
    text: String,
    caller_module: Option<String>,
    module_offset: Option<u64>,
    thread_id: u32,
    timestamp_ms: i64,
}

struct SenderState {
    pid: AtomicU32,
    sender: AtomicPtr<SyncSender<Observation>>,
}

static SENDER: SenderState = SenderState {
    pid: AtomicU32::new(0),
    sender: AtomicPtr::new(std::ptr::null_mut()),
};
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn observe(adapter: Adapter, api: &'static str, text: String) {
    if text.is_empty() {
        return;
    }
    let text = truncate_utf8(text, MAX_TEXT_BYTES);
    let (caller_module, module_offset) = external_callsite();
    let observation = Observation {
        adapter,
        api,
        text,
        caller_module,
        module_offset,
        thread_id: unsafe { libc::syscall(libc::SYS_gettid) as u32 },
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64,
    };
    let pid = std::process::id();
    let observed_pid = SENDER.pid.load(Ordering::Acquire);
    if observed_pid != pid
        && SENDER
            .pid
            .compare_exchange(observed_pid, pid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let (tx, rx) = mpsc::sync_channel(OBSERVATION_QUEUE);
        if std::thread::Builder::new()
            .name("terratranslate-native-hook".into())
            .spawn(move || sender_loop(rx))
            .is_ok()
        {
            // Sender pointers are intentionally retained across replacement. A concurrent hook
            // may still be using the old sender, and a forked child cannot safely coordinate
            // reclamation with vanished threads from its parent.
            let sender = Box::into_raw(Box::new(tx));
            SENDER.sender.store(sender, Ordering::Release);
        } else {
            SENDER.pid.store(0, Ordering::Release);
        }
    }
    let sender = SENDER.sender.load(Ordering::Acquire);
    if !sender.is_null() {
        // SAFETY: installed senders are never freed because hook callbacks can race replacement.
        let _ = unsafe { &*sender }.try_send(observation);
    }
}

fn sender_loop(receiver: mpsc::Receiver<Observation>) {
    loop {
        let Some(config) = read_config() else {
            std::thread::sleep(CONNECT_RETRY);
            continue;
        };
        match UnixStream::connect(&config.socket_path) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
                if run_connection(&mut stream, &config, &receiver).is_err() {
                    std::thread::sleep(CONNECT_RETRY);
                }
            }
            Err(_) => std::thread::sleep(CONNECT_RETRY),
        }
    }
}

fn run_connection(
    stream: &mut UnixStream,
    config: &HookBridgeConfig,
    receiver: &mpsc::Receiver<Observation>,
) -> Result<(), ()> {
    let executable = executable_identity();
    let hello = BridgeMessage::Hello(BridgeHello {
        protocol_version: PROTOCOL_VERSION,
        authentication_token: config.authentication_token().map_err(|_| ())?,
        bridge_id: Uuid::new_v4(),
        platform: HookPlatform::Linux,
        runtime: HookRuntime::Native,
        process_id: std::process::id(),
        architecture: architecture(),
        executable: executable.clone(),
        adapters: vec!["pango".into(), "sdl_ttf".into(), "cairo".into()],
    });
    write_message(stream, &hello)?;
    match read_message(stream).map_err(|_| ())? {
        HostMessage::Accept { protocol_version } if protocol_version == PROTOCOL_VERSION => {}
        _ => return Err(()),
    }

    let mut candidate_ids = HashMap::<StableCandidateKey, Uuid>::new();
    let mut enabled = HashSet::<Uuid>::new();
    let mut pending = HashMap::<String, Pending>::new();
    loop {
        loop {
            match read_message(stream) {
                Ok(HostMessage::EnableCandidate(id)) => {
                    enabled.insert(id);
                }
                Ok(HostMessage::DisableCandidate(id)) => {
                    enabled.remove(&id);
                }
                Ok(HostMessage::Ping(value)) => {
                    write_message(stream, &BridgeMessage::Pong(value))?;
                }
                Ok(HostMessage::Shutdown) | Ok(HostMessage::Reject { .. }) => return Err(()),
                Ok(_) => {}
                Err(ReadError::Timeout) => break,
                Err(ReadError::Disconnected) => return Err(()),
            }
        }
        match receiver.try_recv() {
            Ok(observation) => {
                let stable_key = StableCandidateKey::derive(
                    &HookPlatform::Linux,
                    &executable,
                    observation.adapter.id(),
                    observation.caller_module.as_deref(),
                    observation.module_offset,
                );
                let candidate_id = if let Some(id) = candidate_ids.get(&stable_key) {
                    *id
                } else {
                    if candidate_ids.len() >= MAX_CANDIDATES {
                        continue;
                    }
                    let id = Uuid::new_v4();
                    write_message(
                        stream,
                        &BridgeMessage::Candidate(HookCandidate {
                            candidate_id: id,
                            stable_key: stable_key.clone(),
                            adapter_id: observation.adapter.id().into(),
                            api: observation.api.into(),
                            caller_module: observation.caller_module.clone(),
                            module_offset: observation.module_offset,
                            sample: truncate_utf8(observation.text.clone(), MAX_SAMPLE_BYTES),
                            embeddable: false,
                            metadata: Default::default(),
                        }),
                    )?;
                    candidate_ids.insert(stable_key.clone(), id);
                    id
                };
                if enabled.contains(&candidate_id) {
                    let replace = pending.get(&observation.text).is_none_or(|old| {
                        observation.adapter.priority() > old.observation.adapter.priority()
                    });
                    if replace {
                        if pending.len() >= MAX_PENDING_EVENTS
                            && !pending.contains_key(&observation.text)
                            && let Some(oldest) = pending
                                .iter()
                                .min_by_key(|(_, event)| event.inserted)
                                .map(|(text, _)| text.clone())
                        {
                            pending.remove(&oldest);
                        }
                        pending.insert(
                            observation.text.clone(),
                            Pending {
                                inserted: Instant::now(),
                                observation,
                                stable_key,
                                candidate_id,
                            },
                        );
                    }
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(2)),
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
        let now = Instant::now();
        let ready = pending
            .iter()
            .filter_map(|(text, event)| {
                (now.duration_since(event.inserted) >= DEDUP_WINDOW).then_some(text.clone())
            })
            .collect::<Vec<_>>();
        for text in ready {
            if let Some(event) = pending.remove(&text) {
                write_message(
                    stream,
                    &BridgeMessage::Text(HookTextEvent {
                        sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed),
                        candidate_id: event.candidate_id,
                        stable_key: event.stable_key,
                        thread_id: event.observation.thread_id,
                        timestamp_ms: event.observation.timestamp_ms,
                        text: event.observation.text,
                        speaker: None,
                        replacement_capacity_utf16: None,
                    }),
                )?;
            }
        }
    }
}

struct Pending {
    inserted: Instant,
    observation: Observation,
    stable_key: StableCandidateKey,
    candidate_id: Uuid,
}

fn read_config() -> Option<HookBridgeConfig> {
    let path = std::env::var_os("TERRATRANSLATE_HOOK_CONFIG")?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn executable_identity() -> ExecutableIdentity {
    let path = truncate_utf8(
        std::fs::read_link("/proc/self/exe")
            .unwrap_or_else(|_| Path::new("unknown").to_owned())
            .to_string_lossy()
            .into_owned(),
        MAX_IDENTITY_BYTES,
    );
    ExecutableIdentity {
        path,
        image_id: None,
    }
}

fn architecture() -> ProcessArchitecture {
    #[cfg(target_arch = "x86")]
    return ProcessArchitecture::X86;
    #[cfg(target_arch = "x86_64")]
    return ProcessArchitecture::X86_64;
    #[cfg(target_arch = "aarch64")]
    return ProcessArchitecture::Aarch64;
    #[allow(unreachable_code)]
    ProcessArchitecture::Other(std::env::consts::ARCH.into())
}

fn write_message(stream: &mut UnixStream, message: &BridgeMessage) -> Result<(), ()> {
    let bytes = encode(message).map_err(|_| ())?;
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|_| ())
}

enum ReadError {
    Timeout,
    Disconnected,
}

fn read_message(stream: &mut UnixStream) -> Result<HostMessage, ReadError> {
    let mut length = [0; 4];
    match stream.read_exact(&mut length) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(ReadError::Timeout);
        }
        Err(_) => return Err(ReadError::Disconnected),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_WIRE_MESSAGE_BYTES {
        return Err(ReadError::Disconnected);
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|_| ReadError::Disconnected)?;
    decode(&bytes, MAX_WIRE_MESSAGE_BYTES).map_err(|_| ReadError::Disconnected)
}

fn truncate_utf8(mut text: String, maximum: usize) -> String {
    if text.len() <= maximum {
        return text;
    }
    let mut end = maximum;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn external_callsite() -> (Option<String>, Option<u64>) {
    let mut frames = [std::ptr::null_mut(); 16];
    let count = unsafe { libc::backtrace(frames.as_mut_ptr(), frames.len() as c_int) };
    for address in frames.into_iter().take(count.max(0) as usize).skip(2) {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
            continue;
        }
        let module = unsafe { CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned();
        if module.contains("terratranslate_native_hook") {
            continue;
        }
        let offset = (!info.dli_fbase.is_null())
            .then(|| address as usize as u64 - info.dli_fbase as usize as u64);
        return (Some(truncate_utf8(module, MAX_IDENTITY_BYTES)), offset);
    }
    (None, None)
}

fn resolve(name: &'static [u8]) -> usize {
    unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast()) as usize }
}

struct HookGuard(bool);

impl HookGuard {
    fn enter() -> Self {
        Self(IN_HOOK.with(|active| {
            if active.get() {
                false
            } else {
                active.set(true);
                true
            }
        }))
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        if self.0 {
            IN_HOOK.with(|active| active.set(false));
        }
    }
}

unsafe fn utf8(pointer: *const c_char, length: c_int) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    let bytes = if length < 0 {
        unsafe { CStr::from_ptr(pointer) }.to_bytes()
    } else {
        unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length as usize) }
    };
    let bytes = &bytes[..bytes.len().min(MAX_TEXT_BYTES)];
    Some(String::from_utf8_lossy(bytes).into_owned())
}

unsafe fn utf16(pointer: *const u16) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    let mut length = 0;
    while length < MAX_TEXT_BYTES / 2 && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(pointer, length)
    }))
}

fn safely_observe(adapter: Adapter, api: &'static str, text: Option<String>) {
    let _ = std::panic::catch_unwind(|| {
        if let Some(text) = text {
            observe(adapter, api, text);
        }
    });
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdlColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

macro_rules! sdl_utf8 {
    ($name:ident, $symbol:literal, ($($arg:ident: $ty:ty),*)) => {
        #[unsafe(export_name = $symbol)]
        pub unsafe extern "C" fn $name(font: *mut c_void, text: *const c_char, $($arg: $ty),*) -> *mut c_void {
            type Original = unsafe extern "C" fn(*mut c_void, *const c_char, $($ty),*) -> *mut c_void;
            static ORIGINAL: OnceLock<usize> = OnceLock::new();
            let guard = HookGuard::enter();
            let original: Original = unsafe { std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(concat!($symbol, "\0").as_bytes()))) };
            let result = unsafe { original(font, text, $($arg),*) };
            if guard.0 { safely_observe(Adapter::SdlTtf, $symbol, unsafe { utf8(text, -1) }); }
            result
        }
    };
}

macro_rules! sdl_utf16 {
    ($name:ident, $symbol:literal, ($($arg:ident: $ty:ty),*)) => {
        #[unsafe(export_name = $symbol)]
        pub unsafe extern "C" fn $name(font: *mut c_void, text: *const u16, $($arg: $ty),*) -> *mut c_void {
            type Original = unsafe extern "C" fn(*mut c_void, *const u16, $($ty),*) -> *mut c_void;
            static ORIGINAL: OnceLock<usize> = OnceLock::new();
            let guard = HookGuard::enter();
            let original: Original = unsafe { std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(concat!($symbol, "\0").as_bytes()))) };
            let result = unsafe { original(font, text, $($arg),*) };
            if guard.0 { safely_observe(Adapter::SdlTtf, $symbol, unsafe { utf16(text) }); }
            result
        }
    };
}

sdl_utf8!(ttf_render_utf8_solid, "TTF_RenderUTF8_Solid", (fg: SdlColor));
sdl_utf8!(ttf_render_utf8_shaded, "TTF_RenderUTF8_Shaded", (fg: SdlColor, bg: SdlColor));
sdl_utf8!(ttf_render_utf8_blended, "TTF_RenderUTF8_Blended", (fg: SdlColor));
sdl_utf8!(ttf_render_utf8_blended_wrapped, "TTF_RenderUTF8_Blended_Wrapped", (fg: SdlColor, wrap_length: c_uint));
sdl_utf8!(ttf_render_utf8_solid_wrapped, "TTF_RenderUTF8_Solid_Wrapped", (fg: SdlColor, wrap_length: c_uint));
sdl_utf8!(ttf_render_utf8_shaded_wrapped, "TTF_RenderUTF8_Shaded_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_length: c_uint));
sdl_utf8!(ttf_render_utf8_lcd, "TTF_RenderUTF8_LCD", (fg: SdlColor, bg: SdlColor));
sdl_utf8!(ttf_render_utf8_lcd_wrapped, "TTF_RenderUTF8_LCD_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_length: c_uint));
sdl_utf16!(ttf_render_unicode_solid, "TTF_RenderUNICODE_Solid", (fg: SdlColor));
sdl_utf16!(ttf_render_unicode_shaded, "TTF_RenderUNICODE_Shaded", (fg: SdlColor, bg: SdlColor));
sdl_utf16!(ttf_render_unicode_blended, "TTF_RenderUNICODE_Blended", (fg: SdlColor));
sdl_utf16!(ttf_render_unicode_blended_wrapped, "TTF_RenderUNICODE_Blended_Wrapped", (fg: SdlColor, wrap_length: c_uint));
sdl_utf16!(ttf_render_unicode_solid_wrapped, "TTF_RenderUNICODE_Solid_Wrapped", (fg: SdlColor, wrap_length: c_uint));
sdl_utf16!(ttf_render_unicode_shaded_wrapped, "TTF_RenderUNICODE_Shaded_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_length: c_uint));
sdl_utf16!(ttf_render_unicode_lcd, "TTF_RenderUNICODE_LCD", (fg: SdlColor, bg: SdlColor));
sdl_utf16!(ttf_render_unicode_lcd_wrapped, "TTF_RenderUNICODE_LCD_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_length: c_uint));

macro_rules! sdl3_text {
    ($name:ident, $symbol:literal, ($($arg:ident: $ty:ty),*)) => {
        #[unsafe(export_name = $symbol)]
        pub unsafe extern "C" fn $name(font: *mut c_void, text: *const c_char, length: usize, $($arg: $ty),*) -> *mut c_void {
            type Original = unsafe extern "C" fn(*mut c_void, *const c_char, usize, $($ty),*) -> *mut c_void;
            static ORIGINAL: OnceLock<usize> = OnceLock::new();
            let guard = HookGuard::enter();
            let original: Original = unsafe { std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(concat!($symbol, "\0").as_bytes()))) };
            let result = unsafe { original(font, text, length, $($arg),*) };
            if guard.0 { safely_observe(Adapter::SdlTtf, $symbol, unsafe { utf8(text, length.min(c_int::MAX as usize) as c_int) }); }
            result
        }
    };
}

sdl3_text!(ttf_render_text_solid, "TTF_RenderText_Solid", (fg: SdlColor));
sdl3_text!(ttf_render_text_shaded, "TTF_RenderText_Shaded", (fg: SdlColor, bg: SdlColor));
sdl3_text!(ttf_render_text_blended, "TTF_RenderText_Blended", (fg: SdlColor));
sdl3_text!(ttf_render_text_lcd, "TTF_RenderText_LCD", (fg: SdlColor, bg: SdlColor));
sdl3_text!(ttf_render_text_solid_wrapped, "TTF_RenderText_Solid_Wrapped", (fg: SdlColor, wrap_width: c_int));
sdl3_text!(ttf_render_text_shaded_wrapped, "TTF_RenderText_Shaded_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_width: c_int));
sdl3_text!(ttf_render_text_blended_wrapped, "TTF_RenderText_Blended_Wrapped", (fg: SdlColor, wrap_width: c_int));
sdl3_text!(ttf_render_text_lcd_wrapped, "TTF_RenderText_LCD_Wrapped", (fg: SdlColor, bg: SdlColor, wrap_width: c_int));

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pango_layout_set_text(
    layout: *mut c_void,
    text: *const c_char,
    length: c_int,
) {
    type Original = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);
    static ORIGINAL: OnceLock<usize> = OnceLock::new();
    let guard = HookGuard::enter();
    let original: Original = unsafe {
        std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(b"pango_layout_set_text\0")))
    };
    unsafe { original(layout, text, length) };
    if guard.0 {
        observe_pango(layout, "pango_layout_set_text");
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pango_layout_set_markup(
    layout: *mut c_void,
    markup: *const c_char,
    length: c_int,
) {
    type Original = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);
    static ORIGINAL: OnceLock<usize> = OnceLock::new();
    let guard = HookGuard::enter();
    let original: Original = unsafe {
        std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(b"pango_layout_set_markup\0")))
    };
    unsafe { original(layout, markup, length) };
    if guard.0 {
        observe_pango(layout, "pango_layout_set_markup");
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pango_layout_set_markup_with_accel(
    layout: *mut c_void,
    markup: *const c_char,
    length: c_int,
    accel_marker: u32,
    accel_char: *mut u32,
) {
    type Original = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, u32, *mut u32);
    static ORIGINAL: OnceLock<usize> = OnceLock::new();
    let guard = HookGuard::enter();
    let original: Original = unsafe {
        std::mem::transmute(
            *ORIGINAL.get_or_init(|| resolve(b"pango_layout_set_markup_with_accel\0")),
        )
    };
    unsafe { original(layout, markup, length, accel_marker, accel_char) };
    if guard.0 {
        observe_pango(layout, "pango_layout_set_markup_with_accel");
    }
}

fn observe_pango(layout: *mut c_void, api: &'static str) {
    let _ = std::panic::catch_unwind(|| unsafe {
        type GetText = unsafe extern "C" fn(*mut c_void) -> *const c_char;
        static GET_TEXT: OnceLock<usize> = OnceLock::new();
        let get_text: GetText =
            std::mem::transmute(*GET_TEXT.get_or_init(|| resolve(b"pango_layout_get_text\0")));
        safely_observe(Adapter::Pango, api, utf8(get_text(layout), -1));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cairo_show_text(context: *mut c_void, text: *const c_char) {
    type Original = unsafe extern "C" fn(*mut c_void, *const c_char);
    static ORIGINAL: OnceLock<usize> = OnceLock::new();
    let guard = HookGuard::enter();
    let original: Original =
        unsafe { std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(b"cairo_show_text\0"))) };
    unsafe { original(context, text) };
    if guard.0 {
        safely_observe(Adapter::Cairo, "cairo_show_text", unsafe { utf8(text, -1) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cairo_show_text_glyphs(
    context: *mut c_void,
    utf8_text: *const c_char,
    utf8_length: c_int,
    glyphs: *const c_void,
    num_glyphs: c_int,
    clusters: *const c_void,
    num_clusters: c_int,
    cluster_flags: c_int,
) {
    type Original = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        c_int,
        *const c_void,
        c_int,
        *const c_void,
        c_int,
        c_int,
    );
    static ORIGINAL: OnceLock<usize> = OnceLock::new();
    let guard = HookGuard::enter();
    let original: Original = unsafe {
        std::mem::transmute(*ORIGINAL.get_or_init(|| resolve(b"cairo_show_text_glyphs\0")))
    };
    unsafe {
        original(
            context,
            utf8_text,
            utf8_length,
            glyphs,
            num_glyphs,
            clusters,
            num_clusters,
            cluster_flags,
        )
    };
    if guard.0 {
        safely_observe(Adapter::Cairo, "cairo_show_text_glyphs", unsafe {
            utf8(utf8_text, utf8_length)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_unicode_boundaries() {
        assert_eq!(truncate_utf8("a界b".into(), 3), "a");
        assert_eq!(truncate_utf8("a界b".into(), 4), "a界");
    }

    #[test]
    fn adapter_priority_matches_semantic_quality() {
        assert!(Adapter::Pango.priority() > Adapter::SdlTtf.priority());
        assert!(Adapter::SdlTtf.priority() > Adapter::Cairo.priority());
    }

    #[test]
    fn explicit_utf8_length_does_not_require_nul() {
        let text = "水面extra";
        let captured = unsafe { utf8(text.as_ptr().cast(), "水面".len() as c_int) }.unwrap();
        assert_eq!(captured, "水面");
    }

    #[test]
    fn utf16_copy_is_bounded_and_decodes_unicode() {
        let text = "星空".encode_utf16().chain([0]).collect::<Vec<_>>();
        assert_eq!(unsafe { utf16(text.as_ptr()) }.unwrap(), "星空");
    }
}
