use std::ffi::c_void;
use std::sync::OnceLock;

use minhook::MinHook;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Globalization::{
    CHARSETINFO, FONTSIGNATURE, GetACP, GetTextCharsetInfo, MultiByteToWideChar, TCI_SRCCHARSET,
    TranslateCharsetInfo,
};
use windows_sys::Win32::Graphics::Gdi::{
    DRAWTEXTPARAMS, ETO_GLYPH_INDEX, HDC, POLYTEXTA, POLYTEXTW,
};
use windows_sys::core::BOOL;

use crate::{MAX_TEXT_UTF16, bounded_utf16};

use super::observe;

type TextOutW = unsafe extern "system" fn(HDC, i32, i32, *const u16, i32) -> BOOL;
type TextOutA = unsafe extern "system" fn(HDC, i32, i32, *const u8, i32) -> BOOL;
type ExtTextOutW =
    unsafe extern "system" fn(HDC, i32, i32, u32, *const RECT, *const u16, u32, *const i32) -> BOOL;
type ExtTextOutA =
    unsafe extern "system" fn(HDC, i32, i32, u32, *const RECT, *const u8, u32, *const i32) -> BOOL;
type DrawTextW = unsafe extern "system" fn(HDC, *const u16, i32, *mut RECT, u32) -> i32;
type DrawTextA = unsafe extern "system" fn(HDC, *const u8, i32, *mut RECT, u32) -> i32;
type DrawTextExW =
    unsafe extern "system" fn(HDC, *mut u16, i32, *mut RECT, u32, *const DRAWTEXTPARAMS) -> i32;
type DrawTextExA =
    unsafe extern "system" fn(HDC, *mut u8, i32, *mut RECT, u32, *const DRAWTEXTPARAMS) -> i32;
type PolyTextOutW = unsafe extern "system" fn(HDC, *const POLYTEXTW, i32) -> BOOL;
type PolyTextOutA = unsafe extern "system" fn(HDC, *const POLYTEXTA, i32) -> BOOL;

static TEXT_OUT_W: OnceLock<usize> = OnceLock::new();
static TEXT_OUT_A: OnceLock<usize> = OnceLock::new();
static EXT_TEXT_OUT_W: OnceLock<usize> = OnceLock::new();
static EXT_TEXT_OUT_A: OnceLock<usize> = OnceLock::new();
static DRAW_TEXT_W: OnceLock<usize> = OnceLock::new();
static DRAW_TEXT_A: OnceLock<usize> = OnceLock::new();
static DRAW_TEXT_EX_W: OnceLock<usize> = OnceLock::new();
static DRAW_TEXT_EX_A: OnceLock<usize> = OnceLock::new();
static POLY_TEXT_OUT_W: OnceLock<usize> = OnceLock::new();
static POLY_TEXT_OUT_A: OnceLock<usize> = OnceLock::new();

pub(super) unsafe fn install() {
    unsafe {
        hook("gdi32.dll", "TextOutW", text_out_w as _, &TEXT_OUT_W);
        hook("gdi32.dll", "TextOutA", text_out_a as _, &TEXT_OUT_A);
        hook(
            "gdi32.dll",
            "ExtTextOutW",
            ext_text_out_w as _,
            &EXT_TEXT_OUT_W,
        );
        hook(
            "gdi32.dll",
            "ExtTextOutA",
            ext_text_out_a as _,
            &EXT_TEXT_OUT_A,
        );
        hook("user32.dll", "DrawTextW", draw_text_w as _, &DRAW_TEXT_W);
        hook("user32.dll", "DrawTextA", draw_text_a as _, &DRAW_TEXT_A);
        hook(
            "user32.dll",
            "DrawTextExW",
            draw_text_ex_w as _,
            &DRAW_TEXT_EX_W,
        );
        hook(
            "user32.dll",
            "DrawTextExA",
            draw_text_ex_a as _,
            &DRAW_TEXT_EX_A,
        );
        hook(
            "gdi32.dll",
            "PolyTextOutW",
            poly_text_out_w as _,
            &POLY_TEXT_OUT_W,
        );
        hook(
            "gdi32.dll",
            "PolyTextOutA",
            poly_text_out_a as _,
            &POLY_TEXT_OUT_A,
        );
    }
}

unsafe fn hook(module: &str, name: &str, detour: *mut c_void, original: &OnceLock<usize>) {
    if let Ok(pointer) = unsafe { MinHook::create_hook_api(module, name, detour) } {
        let _ = original.set(pointer as usize);
    }
}

unsafe extern "system" fn text_out_w(
    hdc: HDC,
    x: i32,
    y: i32,
    string: *const u16,
    count: i32,
) -> BOOL {
    capture_wide("TextOutW", string, count.max(0) as usize);
    unsafe { original::<TextOutW>(&TEXT_OUT_W)(hdc, x, y, string, count) }
}

unsafe extern "system" fn text_out_a(
    hdc: HDC,
    x: i32,
    y: i32,
    string: *const u8,
    count: i32,
) -> BOOL {
    capture_ansi(hdc, "TextOutA", string, count.max(0) as usize);
    unsafe { original::<TextOutA>(&TEXT_OUT_A)(hdc, x, y, string, count) }
}

unsafe extern "system" fn ext_text_out_w(
    hdc: HDC,
    x: i32,
    y: i32,
    options: u32,
    rect: *const RECT,
    string: *const u16,
    count: u32,
    dx: *const i32,
) -> BOOL {
    if options & ETO_GLYPH_INDEX == 0 {
        capture_wide("ExtTextOutW", string, count as usize);
    }
    unsafe { original::<ExtTextOutW>(&EXT_TEXT_OUT_W)(hdc, x, y, options, rect, string, count, dx) }
}

unsafe extern "system" fn ext_text_out_a(
    hdc: HDC,
    x: i32,
    y: i32,
    options: u32,
    rect: *const RECT,
    string: *const u8,
    count: u32,
    dx: *const i32,
) -> BOOL {
    if options & ETO_GLYPH_INDEX == 0 {
        capture_ansi(hdc, "ExtTextOutA", string, count as usize);
    }
    unsafe { original::<ExtTextOutA>(&EXT_TEXT_OUT_A)(hdc, x, y, options, rect, string, count, dx) }
}

unsafe extern "system" fn draw_text_w(
    hdc: HDC,
    string: *const u16,
    count: i32,
    rect: *mut RECT,
    format: u32,
) -> i32 {
    capture_wide("DrawTextW", string, wide_length(string, count));
    unsafe { original::<DrawTextW>(&DRAW_TEXT_W)(hdc, string, count, rect, format) }
}

unsafe extern "system" fn draw_text_a(
    hdc: HDC,
    string: *const u8,
    count: i32,
    rect: *mut RECT,
    format: u32,
) -> i32 {
    capture_ansi(hdc, "DrawTextA", string, byte_length(string, count));
    unsafe { original::<DrawTextA>(&DRAW_TEXT_A)(hdc, string, count, rect, format) }
}

unsafe extern "system" fn draw_text_ex_w(
    hdc: HDC,
    string: *mut u16,
    count: i32,
    rect: *mut RECT,
    format: u32,
    parameters: *const DRAWTEXTPARAMS,
) -> i32 {
    capture_wide("DrawTextExW", string, wide_length(string, count));
    unsafe {
        original::<DrawTextExW>(&DRAW_TEXT_EX_W)(hdc, string, count, rect, format, parameters)
    }
}

unsafe extern "system" fn draw_text_ex_a(
    hdc: HDC,
    string: *mut u8,
    count: i32,
    rect: *mut RECT,
    format: u32,
    parameters: *const DRAWTEXTPARAMS,
) -> i32 {
    capture_ansi(hdc, "DrawTextExA", string, byte_length(string, count));
    unsafe {
        original::<DrawTextExA>(&DRAW_TEXT_EX_A)(hdc, string, count, rect, format, parameters)
    }
}

unsafe extern "system" fn poly_text_out_w(hdc: HDC, entries: *const POLYTEXTW, count: i32) -> BOOL {
    if !entries.is_null() {
        for index in 0..count.clamp(0, 128) as usize {
            let entry = unsafe { &*entries.add(index) };
            if entry.uiFlags & ETO_GLYPH_INDEX == 0 {
                capture_wide("PolyTextOutW", entry.lpstr, entry.n as usize);
            }
        }
    }
    unsafe { original::<PolyTextOutW>(&POLY_TEXT_OUT_W)(hdc, entries, count) }
}

unsafe extern "system" fn poly_text_out_a(hdc: HDC, entries: *const POLYTEXTA, count: i32) -> BOOL {
    if !entries.is_null() {
        for index in 0..count.clamp(0, 128) as usize {
            let entry = unsafe { &*entries.add(index) };
            if entry.uiFlags & ETO_GLYPH_INDEX == 0 {
                capture_ansi(hdc, "PolyTextOutA", entry.lpstr, entry.n as usize);
            }
        }
    }
    unsafe { original::<PolyTextOutA>(&POLY_TEXT_OUT_A)(hdc, entries, count) }
}

fn capture_wide(api: &'static str, pointer: *const u16, count: usize) {
    let _ = std::panic::catch_unwind(|| {
        if let Some(text) = unsafe { bounded_utf16(pointer, count) } {
            observe("gdi", api, text);
        }
    });
}

fn capture_ansi(hdc: HDC, api: &'static str, pointer: *const u8, count: usize) {
    let _ = std::panic::catch_unwind(|| {
        if let Some(text) = unsafe { bounded_ansi(pointer, count, code_page(hdc)) } {
            observe("gdi", api, text);
        }
    });
}

fn code_page(hdc: HDC) -> u32 {
    let mut signature = FONTSIGNATURE::default();
    let charset = unsafe { GetTextCharsetInfo(hdc, &mut signature, 0) };
    if charset >= 0 {
        let mut charset_value = charset as u32;
        let mut info = CHARSETINFO::default();
        if unsafe { TranslateCharsetInfo(&mut charset_value, &mut info, TCI_SRCCHARSET) } != 0
            && info.ciACP != 0
        {
            return info.ciACP;
        }
    }
    unsafe { GetACP() }
}

unsafe fn bounded_ansi(pointer: *const u8, count: usize, code_page: u32) -> Option<String> {
    if pointer.is_null() || count == 0 {
        return None;
    }
    let count = count.min(MAX_TEXT_UTF16 * 4).min(i32::MAX as usize);
    let needed = unsafe {
        MultiByteToWideChar(code_page, 0, pointer, count as i32, std::ptr::null_mut(), 0)
    };
    if needed <= 0 {
        return None;
    }
    let mut wide = vec![0_u16; (needed as usize).min(MAX_TEXT_UTF16)];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            pointer,
            count as i32,
            wide.as_mut_ptr(),
            wide.len() as i32,
        )
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(
        String::from_utf16_lossy(&wide)
            .trim_matches('\0')
            .to_owned(),
    )
}

fn wide_length(pointer: *const u16, count: i32) -> usize {
    if count >= 0 {
        return count as usize;
    }
    nul_length(pointer, MAX_TEXT_UTF16)
}

fn byte_length(pointer: *const u8, count: i32) -> usize {
    if count >= 0 {
        return count as usize;
    }
    nul_length(pointer, MAX_TEXT_UTF16 * 4)
}

fn nul_length<T: Default + PartialEq + Copy>(pointer: *const T, maximum: usize) -> usize {
    if pointer.is_null() {
        return 0;
    }
    for length in 0..maximum {
        if unsafe { *pointer.add(length) } == T::default() {
            return length;
        }
    }
    maximum
}

unsafe fn original<T: Copy>(slot: &OnceLock<usize>) -> T {
    unsafe { std::mem::transmute_copy(slot.get().expect("hook original must be installed")) }
}
