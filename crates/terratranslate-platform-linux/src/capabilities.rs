use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayServer {
    X11,
    Wayland,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopCapabilities {
    pub display_server: DisplayServer,
    pub flatpak: bool,
    pub native_preload_launch_possible: bool,
    pub wine_attach_possible: bool,
    pub portal_capture_possible: bool,
    pub portal_shortcuts_possible: bool,
    pub consuming_shortcuts_possible: bool,
    pub hud_possible: bool,
    pub spatial_overlay_possible: bool,
    pub diagnostics: Vec<String>,
}

impl DesktopCapabilities {
    pub fn detect() -> Self {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let display_server = if session_type.eq_ignore_ascii_case("wayland")
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
        {
            DisplayServer::Wayland
        } else if session_type.eq_ignore_ascii_case("x11") || std::env::var_os("DISPLAY").is_some()
        {
            DisplayServer::X11
        } else {
            DisplayServer::Unknown
        };
        let portal = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        let flatpak = std::env::var_os("FLATPAK_ID").is_some()
            || std::path::Path::new("/.flatpak-info").exists();
        let mut diagnostics = Vec::new();
        match display_server {
            DisplayServer::Wayland => diagnostics.push(
                "Wayland capture and shortcuts require compositor portal approval; a movable/resizable HUD is available, but arbitrary target-relative overlay positioning is not guaranteed."
                    .into(),
            ),
            DisplayServer::X11 => diagnostics.push(
                "X11 supports passive key grabs and target-relative overlay positioning.".into(),
            ),
            DisplayServer::Unknown => diagnostics.push(
                "No supported graphical session was detected; desktop features are disabled.".into(),
            ),
        }
        if flatpak {
            diagnostics.push(
                "Native LD_PRELOAD launch and Wine process attachment are unavailable in the Flatpak. Install and run the host package to use semantic API hooks; portal vision and AT-SPI remain available."
                    .into(),
            );
        }
        Self {
            display_server,
            flatpak,
            native_preload_launch_possible: !flatpak,
            wine_attach_possible: !flatpak,
            portal_capture_possible: portal && display_server != DisplayServer::Unknown,
            portal_shortcuts_possible: portal && display_server == DisplayServer::Wayland,
            consuming_shortcuts_possible: display_server == DisplayServer::X11
                || (portal && display_server == DisplayServer::Wayland),
            hud_possible: display_server != DisplayServer::Unknown,
            spatial_overlay_possible: display_server == DisplayServer::X11,
            diagnostics,
        }
    }
}
