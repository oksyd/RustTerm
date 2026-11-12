//! Caption operations that GPUI 0.2.2 does not implement consistently across platforms.
use gpui::Window;

#[cfg(windows)]
mod windows;

/// A fullscreen window must first leave fullscreen through GPUI, which owns
/// its saved styles and bounds. Maximizing/restoring is a separate operation.
pub fn toggle_size(window: &Window) -> bool {
    if window.is_fullscreen() {
        window.toggle_fullscreen();
        return true;
    }
    #[cfg(windows)]
    {
        windows::toggle_maximize(window)
    }
    #[cfg(not(windows))]
    {
        window.zoom_window();
        true
    }
}

pub fn start_move(window: &Window) -> bool {
    if window.is_fullscreen() {
        return true;
    }
    #[cfg(windows)]
    {
        windows::start_move(window)
    }
    #[cfg(not(windows))]
    {
        window.start_window_move();
        true
    }
}
