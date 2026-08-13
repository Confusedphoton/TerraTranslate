use std::env;

use libloading::Library;
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::prelude::*;
use serde::{Deserialize, Serialize};
use terratranslate_platform_linux::{DesktopCapabilities, DisplayServer};

// GLib's gboolean is a 32-bit integer, rather than Rust's one-byte bool.
type IsLayerShellSupported = unsafe extern "C" fn() -> i32;
type InitLayerShell = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow);
type SetLayer = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32);

const GTK_LAYER_SHELL_LAYER_OVERLAY: i32 = 3;
const GTK_LAYER_SHELL_LIBRARIES: [&str; 2] = ["libgtk4-layer-shell.so.0", "libgtk4-layer-shell.so"];
const WAYLAND_OVERLAY_ENV: &str = "TERRATRANSLATE_WAYLAND_OVERLAY";

/// User-configurable visual treatment for the translation surface.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HudAppearance {
    pub background_color: String,
    pub text_color: String,
    pub background_opacity: f64,
    pub font_family: String,
    pub font_size_pt: f64,
}

impl Default for HudAppearance {
    fn default() -> Self {
        Self {
            background_color: "#1e1e2e".into(),
            text_color: "#ffffff".into(),
            background_opacity: 0.88,
            font_family: "Sans".into(),
            font_size_pt: 18.0,
        }
    }
}

impl HudAppearance {
    pub fn validate(&self) -> Result<(), String> {
        self.css().map(|_| ())
    }

    fn css(&self) -> Result<String, String> {
        let background = parse_hex_color(&self.background_color)?;
        let text = parse_hex_color(&self.text_color)?;
        if !(0.0..=1.0).contains(&self.background_opacity) {
            return Err("background transparency must be between 0% and 100%".into());
        }
        if !(6.0..=96.0).contains(&self.font_size_pt) {
            return Err("font size must be between 6 and 96 pt".into());
        }
        let font_family = escape_css_string(&self.font_family);
        if font_family.is_empty() {
            return Err("font family cannot be empty".into());
        }

        Ok(format!(
            "window.terratranslate-hud, window.terratranslate-hud > .background {{ background-color: transparent; }}\n\
             .terratranslate-hud-content {{ background-color: rgba({}, {}, {}, {:.3}); }}\n\
             .terratranslate-hud-text {{ color: rgb({}, {}, {}); font-family: \"{}\"; font-size: {:.1}pt; }}",
            background.0,
            background.1,
            background.2,
            self.background_opacity,
            text.0,
            text.1,
            text.2,
            font_family,
            self.font_size_pt,
        ))
    }
}

fn parse_hex_color(value: &str) -> Result<(u8, u8, u8), String> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| "colors must use the #RRGGBB format".to_owned())?;
    if hex.len() != 6 {
        return Err("colors must use the #RRGGBB format".into());
    }
    let component = |range| {
        u8::from_str_radix(&hex[range], 16)
            .map_err(|_| "colors must use hexadecimal #RRGGBB values".to_owned())
    };
    Ok((component(0..2)?, component(2..4)?, component(4..6)?))
}

fn escape_css_string(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .flat_map(|character| match character {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            _ => vec![character],
        })
        .collect()
}

/// Optional layer-shell support keeps the ordinary GTK HUD usable on systems that do not package
/// gtk4-layer-shell, while allowing Wayland compositors to place the HUD in their overlay layer
/// when startup has preloaded the runtime.
struct LayerShell {
    _library: Library,
    is_supported: IsLayerShellSupported,
    init_for_window: InitLayerShell,
    set_layer: SetLayer,
}

impl LayerShell {
    fn available_library() -> Option<&'static str> {
        GTK_LAYER_SHELL_LIBRARIES.into_iter().find(|library_name| {
            // SAFETY: This only probes whether an optional system library can be loaded. The
            // handle is dropped immediately; startup re-executes with the selected library
            // preloaded before GTK initializes.
            unsafe { Library::new(*library_name).is_ok() }
        })
    }

    fn load() -> Option<Self> {
        GTK_LAYER_SHELL_LIBRARIES
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

pub(super) fn available_layer_shell_library() -> Option<&'static str> {
    LayerShell::available_library()
}

/// Layer-shell surfaces are not ordinary toplevels, so compositors do not generally provide their
/// normal move and resize controls. Keep that behavior an explicit user choice.
pub(super) fn wayland_overlay_requested() -> bool {
    env::var_os(WAYLAND_OVERLAY_ENV).is_some_and(|value| value == "1")
}

/// A standalone translation surface.
///
/// This intentionally remains a normal GTK top-level window. That gives Wayland users a
/// compositor-compatible HUD that they can move and resize, even when the compositor does not
/// expose a protocol for attaching a surface to another application's coordinates.
pub struct HudWindow {
    window: gtk::Window,
    label: gtk::Label,
    appearance_provider: gtk::CssProvider,
    _layer_shell: Option<LayerShell>,
}

impl HudWindow {
    pub fn new(
        parent: &gtk::ApplicationWindow,
        capabilities: &DesktopCapabilities,
        appearance: &HudAppearance,
    ) -> Self {
        let label = gtk::Label::new(None);
        label.set_wrap(true);
        label.set_selectable(true);
        label.set_halign(gtk::Align::Start);
        label.set_valign(gtk::Align::Center);
        label.set_hexpand(true);
        label.set_vexpand(true);
        label.set_margin_all(18);
        label.add_css_class("title-2");
        label.add_css_class("terratranslate-hud-text");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("terratranslate-hud-content");
        content.append(&label);

        let window = gtk::Window::builder()
            .title("TerraTranslate HUD")
            .default_width(560)
            .default_height(180)
            .resizable(true)
            .hide_on_close(true)
            .build();
        window.add_css_class("terratranslate-hud");
        window.set_child(Some(&content));
        window.set_application(parent.application().as_ref());
        window.set_transient_for(Some(parent));
        window.set_destroy_with_parent(true);

        let layer_shell = if capabilities.display_server == DisplayServer::Wayland
            && wayland_overlay_requested()
        {
            LayerShell::load().and_then(|layer_shell| layer_shell.configure(&window))
        } else {
            None
        };

        let message = match capabilities.display_server {
            DisplayServer::Wayland => {
                if layer_shell.is_some() {
                    "Translations will stream here.\n\nWayland HUD: compositor overlay layer requested.\nLayer surfaces are not manually movable or resizable."
                } else {
                    "Translations will stream here.\n\nWayland HUD: move or resize this window, then choose Use as overlay."
                }
            }
            DisplayServer::X11 => {
                "Translations will stream here.\n\nX11 HUD: move or resize this window, then choose Use as overlay."
            }
            DisplayServer::Unknown => "No graphical session is available for the translation HUD.",
        };
        label.set_text(message);

        let appearance_provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("GTK display is available"),
            &appearance_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        appearance_provider.load_from_data(
            &appearance
                .css()
                .expect("default or persisted HUD appearance must be valid"),
        );

        // Presenting gives the HUD an initial raise. The transient relationship keeps it above
        // the control window on compositors that honor transient stacking, while retaining the
        // regular toplevel semantics required for Wayland move/resize support.
        window.present();

        Self {
            window,
            label,
            appearance_provider,
            _layer_shell: layer_shell,
        }
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Switch between a decorated window for placement and a frameless translation overlay.
    ///
    /// A Wayland layer surface cannot be converted back into a regular movable toplevel after it
    /// has been realized. The ordinary Wayland HUD and the X11 HUD can change modes in place, so
    /// their compositor-managed position and size are retained.
    pub fn set_positioning(&self, positioning: bool) -> bool {
        if !self.supports_positioning() {
            return false;
        }
        self.window.set_decorated(positioning);
        self.window.set_resizable(positioning);
        self.window.present();
        true
    }

    pub fn supports_positioning(&self) -> bool {
        self._layer_shell.is_none()
    }

    pub fn connect_visible_changed<F>(&self, callback: F)
    where
        F: Fn(bool) + 'static,
    {
        self.window
            .connect_visible_notify(move |window| callback(window.is_visible()));
    }

    pub fn set_message(&self, message: &str) {
        self.label.set_text(message);
    }

    pub fn set_appearance(&self, appearance: &HudAppearance) -> Result<(), String> {
        self.appearance_provider.load_from_data(&appearance.css()?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::HudAppearance;

    #[test]
    fn appearance_generates_css_for_valid_values() {
        let css = HudAppearance::default().css().expect("default is valid");
        assert!(css.contains("rgba(30, 30, 46, 0.880)"));
        assert!(css.contains("font-size: 18.0pt"));
    }

    #[test]
    fn appearance_rejects_invalid_color() {
        let appearance = HudAppearance {
            text_color: "white".into(),
            ..HudAppearance::default()
        };
        assert!(appearance.css().is_err());
    }
}
