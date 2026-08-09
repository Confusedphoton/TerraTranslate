use libloading::Library;
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::prelude::*;
use terratranslate_platform_linux::{DesktopCapabilities, DisplayServer};

// GLib's gboolean is a 32-bit integer, rather than Rust's one-byte bool.
type IsLayerShellSupported = unsafe extern "C" fn() -> i32;
type InitLayerShell = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow);
type SetLayer = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32);

const GTK_LAYER_SHELL_LAYER_OVERLAY: i32 = 3;

/// Dynamically loaded layer-shell support keeps the ordinary GTK HUD usable on systems that do
/// not package gtk4-layer-shell, while allowing Wayland compositors to place the HUD in their
/// overlay layer when the optional runtime is present.
struct LayerShell {
    _library: Library,
    is_supported: IsLayerShellSupported,
    init_for_window: InitLayerShell,
    set_layer: SetLayer,
}

impl LayerShell {
    fn load() -> Option<Self> {
        ["libgtk4-layer-shell.so.0", "libgtk4-layer-shell.so"]
            .into_iter()
            .find_map(|library_name| {
                // SAFETY: Loading an optional, system-provided shared library is isolated to the
                // HUD setup path. The library is retained for the lifetime of the HUD window.
                let library = unsafe { Library::new(library_name).ok()? };
                // SAFETY: These are the stable gtk4-layer-shell C entry points. Copying the
                // function pointers lets us retain the library without borrowing it.
                let is_supported = unsafe {
                    *library
                        .get::<IsLayerShellSupported>(b"gtk_layer_is_supported\0")
                        .ok()?
                };
                let init_for_window = unsafe {
                    *library
                        .get::<InitLayerShell>(b"gtk_layer_init_for_window\0")
                        .ok()?
                };
                let set_layer = unsafe { *library.get::<SetLayer>(b"gtk_layer_set_layer\0").ok()? };
                Some(Self {
                    _library: library,
                    is_supported,
                    init_for_window,
                    set_layer,
                })
            })
    }

    fn configure(self, window: &gtk::Window) -> Option<Self> {
        // SAFETY: `window` is a live GTK window and all function pointers came from the retained
        // gtk4-layer-shell library. Initialization happens before the first present/realize.
        unsafe {
            if (self.is_supported)() == 0 {
                return None;
            }
            (self.init_for_window)(window.as_ptr() as *mut gtk::ffi::GtkWindow);
            (self.set_layer)(
                window.as_ptr() as *mut gtk::ffi::GtkWindow,
                GTK_LAYER_SHELL_LAYER_OVERLAY,
            );
        }
        Some(self)
    }
}

/// A standalone translation surface.
///
/// This intentionally remains a normal GTK top-level window. That gives Wayland users a
/// compositor-compatible HUD that they can move and resize, even when the compositor does not
/// expose a protocol for attaching a surface to another application's coordinates.
pub struct HudWindow {
    window: gtk::Window,
    label: gtk::Label,
    _layer_shell: Option<LayerShell>,
}

impl HudWindow {
    pub fn new(parent: &gtk::ApplicationWindow, capabilities: &DesktopCapabilities) -> Self {
        let label = gtk::Label::new(None);
        label.set_wrap(true);
        label.set_selectable(true);
        label.set_halign(gtk::Align::Start);
        label.set_valign(gtk::Align::Center);
        label.set_hexpand(true);
        label.set_vexpand(true);
        label.set_margin_all(18);
        label.add_css_class("title-2");

        let window = gtk::Window::builder()
            .title("TerraTranslate HUD")
            .default_width(560)
            .default_height(180)
            .resizable(true)
            .hide_on_close(true)
            .build();
        window.set_child(Some(&label));
        window.set_application(parent.application().as_ref());
        window.set_transient_for(Some(parent));
        window.set_destroy_with_parent(true);

        let layer_shell = if capabilities.display_server == DisplayServer::Wayland {
            LayerShell::load().and_then(|layer_shell| layer_shell.configure(&window))
        } else {
            None
        };

        let message = match capabilities.display_server {
            DisplayServer::Wayland => {
                if layer_shell.is_some() {
                    "Translations will stream here.\n\nWayland HUD: compositor overlay layer requested.\nResize behavior is compositor-dependent."
                } else {
                    "Translations will stream here.\n\nWayland HUD: move or resize this window manually.\nInstall gtk4-layer-shell for a compositor overlay-layer request."
                }
            }
            DisplayServer::X11 => {
                "Translations will stream here.\n\nX11 HUD: move or resize this window as needed."
            }
            DisplayServer::Unknown => "No graphical session is available for the translation HUD.",
        };
        label.set_text(message);

        // Presenting gives the HUD an initial raise. The transient relationship keeps it above
        // the control window on compositors that honor transient stacking, while retaining the
        // regular toplevel semantics required for Wayland move/resize support.
        window.present();

        Self {
            window,
            label,
            _layer_shell: layer_shell,
        }
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn set_message(&self, message: &str) {
        self.label.set_text(message);
    }
}
