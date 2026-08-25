use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use minhook::MinHook;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

use crate::bounded_utf16;

use super::observe;

type HResult = i32;
type ScriptStringAnalyse = unsafe extern "system" fn(
    HDC,
    *const c_void,
    i32,
    i32,
    u32,
    u32,
    i32,
    *const c_void,
    *const c_void,
    *const i32,
    *const c_void,
    *const u8,
    *mut *mut c_void,
) -> HResult;
type ScriptStringOut =
    unsafe extern "system" fn(*mut c_void, i32, i32, u32, *const RECT, i32, i32, BOOL) -> HResult;
type ScriptStringFree = unsafe extern "system" fn(*mut *mut c_void) -> HResult;
type ScriptShape = unsafe extern "system" fn(
    HDC,
    *mut *mut c_void,
    *const u16,
    i32,
    i32,
    *mut c_void,
    *mut u16,
    *mut u16,
    *mut c_void,
    *mut i32,
) -> HResult;
type ScriptTextOut = unsafe extern "system" fn(
    HDC,
    *mut *mut c_void,
    i32,
    i32,
    u32,
    *const RECT,
    *const c_void,
    *const u16,
    i32,
    *const u16,
    i32,
    *const i32,
    *const i32,
    *const c_void,
) -> HResult;
type BOOL = i32;

static STRING_ANALYSE: OnceLock<usize> = OnceLock::new();
static STRING_OUT: OnceLock<usize> = OnceLock::new();
static STRING_FREE: OnceLock<usize> = OnceLock::new();
static SHAPE: OnceLock<usize> = OnceLock::new();
static TEXT_OUT: OnceLock<usize> = OnceLock::new();
static ASSOCIATIONS: OnceLock<Mutex<BTreeMap<usize, String>>> = OnceLock::new();

const MAX_ASSOCIATIONS: usize = 256;

pub(super) unsafe fn install() {
    let module = "usp10.dll"
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    if unsafe { LoadLibraryW(module.as_ptr()) }.is_null() {
        return;
    }
    unsafe {
        hook(
            "ScriptStringAnalyse",
            script_string_analyse as _,
            &STRING_ANALYSE,
        );
        hook("ScriptStringOut", script_string_out as _, &STRING_OUT);
        hook("ScriptStringFree", script_string_free as _, &STRING_FREE);
        hook("ScriptShape", script_shape as _, &SHAPE);
        hook("ScriptTextOut", script_text_out as _, &TEXT_OUT);
    }
}

unsafe fn hook(name: &str, detour: *mut c_void, original: &OnceLock<usize>) {
    if let Ok(pointer) = unsafe { MinHook::create_hook_api("usp10.dll", name, detour) } {
        let _ = original.set(pointer as usize);
    }
}

unsafe extern "system" fn script_string_analyse(
    hdc: HDC,
    string: *const c_void,
    count: i32,
    glyphs: i32,
    charset: u32,
    flags: u32,
    width: i32,
    control: *const c_void,
    state: *const c_void,
    dx: *const i32,
    tabs: *const c_void,
    in_class: *const u8,
    analysis: *mut *mut c_void,
) -> HResult {
    let text = unsafe { bounded_utf16(string.cast(), count.max(0) as usize) };
    let result = unsafe {
        original::<ScriptStringAnalyse>(&STRING_ANALYSE)(
            hdc, string, count, glyphs, charset, flags, width, control, state, dx, tabs, in_class,
            analysis,
        )
    };
    if result >= 0 && !analysis.is_null() {
        if let Some(text) = text {
            let key = (unsafe { *analysis }) as usize;
            associate(key, text);
        }
    }
    result
}

unsafe extern "system" fn script_string_out(
    analysis: *mut c_void,
    x: i32,
    y: i32,
    options: u32,
    rect: *const RECT,
    minimum: i32,
    maximum: i32,
    disabled: BOOL,
) -> HResult {
    emit_association("ScriptStringOut", analysis as usize);
    unsafe {
        original::<ScriptStringOut>(&STRING_OUT)(
            analysis, x, y, options, rect, minimum, maximum, disabled,
        )
    }
}

unsafe extern "system" fn script_string_free(analysis: *mut *mut c_void) -> HResult {
    let key = if analysis.is_null() {
        0
    } else {
        (unsafe { *analysis }) as usize
    };
    let result = unsafe { original::<ScriptStringFree>(&STRING_FREE)(analysis) };
    if let Some(mut associations) = ASSOCIATIONS.get().and_then(|values| values.try_lock().ok()) {
        associations.remove(&key);
    }
    result
}

unsafe extern "system" fn script_shape(
    hdc: HDC,
    cache: *mut *mut c_void,
    chars: *const u16,
    char_count: i32,
    max_glyphs: i32,
    analysis: *mut c_void,
    glyphs: *mut u16,
    clusters: *mut u16,
    attributes: *mut c_void,
    glyph_count: *mut i32,
) -> HResult {
    let text = unsafe { bounded_utf16(chars, char_count.max(0) as usize) };
    let result = unsafe {
        original::<ScriptShape>(&SHAPE)(
            hdc,
            cache,
            chars,
            char_count,
            max_glyphs,
            analysis,
            glyphs,
            clusters,
            attributes,
            glyph_count,
        )
    };
    if result >= 0 {
        if let Some(text) = text {
            associate(glyphs as usize, text);
        }
    }
    result
}

unsafe extern "system" fn script_text_out(
    hdc: HDC,
    cache: *mut *mut c_void,
    x: i32,
    y: i32,
    options: u32,
    rect: *const RECT,
    analysis: *const c_void,
    reserved: *const u16,
    reserved_count: i32,
    glyphs: *const u16,
    glyph_count: i32,
    advances: *const i32,
    justify: *const i32,
    offsets: *const c_void,
) -> HResult {
    // `glyphs` are never decoded. Text is emitted only when it can be associated
    // with an earlier ScriptShape call.
    emit_association("ScriptTextOut", glyphs as usize);
    unsafe {
        original::<ScriptTextOut>(&TEXT_OUT)(
            hdc,
            cache,
            x,
            y,
            options,
            rect,
            analysis,
            reserved,
            reserved_count,
            glyphs,
            glyph_count,
            advances,
            justify,
            offsets,
        )
    }
}

fn associate(key: usize, text: String) {
    if key == 0 || text.is_empty() {
        return;
    }
    let associations = ASSOCIATIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let Ok(mut associations) = associations.try_lock() else {
        return;
    };
    if associations.len() >= MAX_ASSOCIATIONS && !associations.contains_key(&key) {
        if let Some(first) = associations.keys().next().copied() {
            associations.remove(&first);
        }
    }
    associations.insert(key, text);
}

fn emit_association(api: &'static str, key: usize) {
    let _ = std::panic::catch_unwind(|| {
        let text = ASSOCIATIONS
            .get()
            .and_then(|values| values.try_lock().ok())
            .and_then(|values| values.get(&key).cloned());
        if let Some(text) = text {
            observe("uniscribe", api, text);
        }
    });
}

unsafe fn original<T: Copy>(slot: &OnceLock<usize>) -> T {
    unsafe { std::mem::transmute_copy(slot.get().expect("hook original must be installed")) }
}
