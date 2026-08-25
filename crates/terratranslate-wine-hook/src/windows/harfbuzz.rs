use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::OnceLock;

use minhook::MinHook;
use terratranslate_wine_protocol::MAX_TEXT_BYTES;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use super::observe;

type HbBufferAddUtf32 = unsafe extern "C" fn(*mut c_void, *const u32, i32, u32, i32);

static BUFFER_ADD_UTF32: OnceLock<usize> = OnceLock::new();

/// Finds HarfBuzz in any loaded PE module and hooks its complete UTF-32 input
/// before glyph shaping. This covers applications that bypass Windows text APIs
/// and render the shaped glyphs through OpenGL, Vulkan, Direct3D, or another
/// custom renderer, whether HarfBuzz is a standalone DLL or bundled into a
/// framework module.
pub(super) unsafe fn install() {
    let snapshot = unsafe {
        CreateToolhelp32Snapshot(
            TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32,
            GetCurrentProcessId(),
        )
    };
    if snapshot == INVALID_HANDLE_VALUE {
        return;
    }
    let mut entry: MODULEENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    if unsafe { Module32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            if let Some(target) =
                unsafe { GetProcAddress(entry.hModule, c"hb_buffer_add_utf32".as_ptr().cast()) }
            {
                let target = target as *const () as *mut c_void;
                if let Ok(pointer) =
                    unsafe { MinHook::create_hook(target, hb_buffer_add_utf32 as _) }
                {
                    let _ = BUFFER_ADD_UTF32.set(pointer as usize);
                    break;
                }
            }
            if unsafe { Module32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
}

unsafe extern "C" fn hb_buffer_add_utf32(
    buffer: *mut c_void,
    text: *const u32,
    text_length: i32,
    item_offset: u32,
    item_length: i32,
) {
    capture(text, text_length, item_offset, item_length);
    unsafe {
        original::<HbBufferAddUtf32>(&BUFFER_ADD_UTF32)(
            buffer,
            text,
            text_length,
            item_offset,
            item_length,
        )
    }
}

fn capture(text: *const u32, text_length: i32, item_offset: u32, item_length: i32) {
    if text.is_null() {
        return;
    }
    // One UTF-32 scalar can expand to four UTF-8 bytes on the wire.
    let maximum = MAX_TEXT_BYTES / 4;
    let total = if text_length < 0 {
        unsafe { nul_terminated_length(text, maximum) }
    } else {
        (text_length as usize).min(maximum)
    };
    let offset = (item_offset as usize).min(total);
    let available = total - offset;
    let length = if item_length < 0 {
        available
    } else {
        (item_length as usize).min(available)
    };
    if length == 0 {
        return;
    }
    // SAFETY: HarfBuzz requires `text` to be readable for `text_length` UTF-32
    // units. Offset and length are clamped to both that range and the wire cap.
    let units = unsafe { std::slice::from_raw_parts(text.add(offset), length) };
    observe(
        "harfbuzz",
        "hb_buffer_add_utf32",
        units
            .iter()
            .map(|unit| char::from_u32(*unit).unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
    );
}

unsafe fn nul_terminated_length(pointer: *const u32, maximum: usize) -> usize {
    let mut length = 0;
    while length < maximum && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    length
}

unsafe fn original<T: Copy>(slot: &OnceLock<usize>) -> T {
    unsafe { std::mem::transmute_copy(slot.get().expect("hook original must be installed")) }
}
