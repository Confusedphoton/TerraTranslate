use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use minhook::MinHook;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
use windows_sys::core::GUID;

use crate::bounded_utf16;

use super::observe;

type HResult = i32;
type DWriteCreateFactory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HResult;
type CreateTextLayout = unsafe extern "system" fn(
    *mut c_void,
    *const u16,
    u32,
    *mut c_void,
    f32,
    f32,
    *mut *mut c_void,
) -> HResult;
type DrawLayout =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, f32, f32) -> HResult;

static CREATE_FACTORY: OnceLock<usize> = OnceLock::new();
static CREATE_LAYOUT: OnceLock<usize> = OnceLock::new();
static DRAW_LAYOUT: OnceLock<usize> = OnceLock::new();
static LAYOUT_TEXT: OnceLock<Mutex<BTreeMap<usize, String>>> = OnceLock::new();

const CREATE_TEXT_LAYOUT_VTABLE_INDEX: usize = 18;
// IDWriteTextLayout inherits the 19 IDWriteTextFormat/IUnknown entries;
// IDWriteTextLayout::Draw is its thirty-first entry (zero-based slot 49).
// IDWriteTextLayout inherits the 28 IDWriteTextFormat/IUnknown entries and Draw is
// the 31st layout method (zero-based vtable slot 58).
const DRAW_VTABLE_INDEX: usize = 58;
const MAX_LAYOUTS: usize = 256;

pub(super) unsafe fn install() {
    let module: Vec<u16> = "dwrite.dll".encode_utf16().chain(Some(0)).collect();
    // This runs on the exported-startup worker, never in DllMain.
    if unsafe { LoadLibraryW(module.as_ptr()) }.is_null() {
        return;
    }
    if let Ok(pointer) = unsafe {
        MinHook::create_hook_api(
            "dwrite.dll",
            "DWriteCreateFactory",
            dwrite_create_factory as _,
        )
    } {
        let _ = CREATE_FACTORY.set(pointer as usize);
    }
}

unsafe extern "system" fn dwrite_create_factory(
    factory_type: u32,
    iid: *const GUID,
    factory: *mut *mut c_void,
) -> HResult {
    let result =
        unsafe { original::<DWriteCreateFactory>(&CREATE_FACTORY)(factory_type, iid, factory) };
    if result >= 0 && !factory.is_null() && !unsafe { *factory }.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe { hook_create_layout(*factory) });
    }
    result
}

unsafe fn hook_create_layout(factory: *mut c_void) {
    if CREATE_LAYOUT.get().is_some() {
        return;
    }
    let target = unsafe { vtable_entry(factory, CREATE_TEXT_LAYOUT_VTABLE_INDEX) };
    if let Ok(pointer) = unsafe { MinHook::create_hook(target, create_text_layout as _) } {
        if CREATE_LAYOUT.set(pointer as usize).is_ok() {
            let _ = unsafe { MinHook::enable_hook(target) };
        }
    }
}

unsafe extern "system" fn create_text_layout(
    factory: *mut c_void,
    text: *const u16,
    length: u32,
    format: *mut c_void,
    maximum_width: f32,
    maximum_height: f32,
    layout: *mut *mut c_void,
) -> HResult {
    let copied = unsafe { bounded_utf16(text, length as usize) };
    let result = unsafe {
        original::<CreateTextLayout>(&CREATE_LAYOUT)(
            factory,
            text,
            length,
            format,
            maximum_width,
            maximum_height,
            layout,
        )
    };
    if result >= 0 && !layout.is_null() && !unsafe { *layout }.is_null() {
        if let Some(text) = copied {
            associate(unsafe { *layout } as usize, text);
        }
        let _ = std::panic::catch_unwind(|| unsafe { hook_draw(*layout) });
    }
    result
}

unsafe fn hook_draw(layout: *mut c_void) {
    if DRAW_LAYOUT.get().is_some() {
        return;
    }
    let target = unsafe { vtable_entry(layout, DRAW_VTABLE_INDEX) };
    if let Ok(pointer) = unsafe { MinHook::create_hook(target, draw_layout as _) } {
        if DRAW_LAYOUT.set(pointer as usize).is_ok() {
            let _ = unsafe { MinHook::enable_hook(target) };
        }
    }
}

unsafe extern "system" fn draw_layout(
    layout: *mut c_void,
    drawing_context: *mut c_void,
    renderer: *mut c_void,
    origin_x: f32,
    origin_y: f32,
) -> HResult {
    let _ = std::panic::catch_unwind(|| {
        let text = LAYOUT_TEXT
            .get()
            .and_then(|values| values.try_lock().ok())
            .and_then(|values| values.get(&(layout as usize)).cloned());
        if let Some(text) = text {
            observe("directwrite", "IDWriteTextLayout::Draw", text);
        }
    });
    unsafe {
        original::<DrawLayout>(&DRAW_LAYOUT)(layout, drawing_context, renderer, origin_x, origin_y)
    }
}

fn associate(layout: usize, text: String) {
    let values = LAYOUT_TEXT.get_or_init(|| Mutex::new(BTreeMap::new()));
    let Ok(mut values) = values.try_lock() else {
        return;
    };
    if values.len() >= MAX_LAYOUTS && !values.contains_key(&layout) {
        if let Some(first) = values.keys().next().copied() {
            values.remove(&first);
        }
    }
    values.insert(layout, text);
}

unsafe fn vtable_entry(instance: *mut c_void, index: usize) -> *mut c_void {
    let vtable = unsafe { *(instance as *mut *mut *mut c_void) };
    unsafe { *vtable.add(index) }
}

unsafe fn original<T: Copy>(slot: &OnceLock<usize>) -> T {
    unsafe { std::mem::transmute_copy(slot.get().expect("hook original must be installed")) }
}
