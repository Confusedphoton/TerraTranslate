//! Linux desktop capability detection and portal-backed integration.

mod capabilities;
mod capture;
mod native_accessibility;
mod native_launch;
mod pipewire_audio;
mod pipewire_video;
mod shortcuts;
mod wine_bridge;
mod wine_targets;

pub use capabilities::*;
pub use capture::*;
pub use native_accessibility::*;
pub use native_launch::*;
pub use pipewire_audio::*;
pub use pipewire_video::*;
pub use shortcuts::*;
pub use wine_bridge::*;
pub use wine_targets::*;
