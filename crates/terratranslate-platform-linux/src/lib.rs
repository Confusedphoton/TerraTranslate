//! Linux desktop capability detection and portal-backed integration.

mod capabilities;
mod capture;
mod pipewire_audio;
mod pipewire_video;
mod shortcuts;
mod wine_bridge;

pub use capabilities::*;
pub use capture::*;
pub use pipewire_audio::*;
pub use pipewire_video::*;
pub use shortcuts::*;
pub use wine_bridge::*;
