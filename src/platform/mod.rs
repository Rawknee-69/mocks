pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux_x11;

#[cfg(target_os = "linux")]
pub mod linux_wayland;

/// Returns the primary monitor's current refresh rate in Hz.
/// Uses platform-specific APIs; falls back to 60 Hz.
pub fn query_refresh_hz() -> u32 {
    #[cfg(target_os = "windows")]
    {
        windows::query_refresh_hz()
    }
    #[cfg(not(target_os = "windows"))]
    {
        60
    }
}

/// Block until the next display vsync boundary.
/// Call this at the START of each frame iteration, before sampling the cursor.
/// On Windows: DwmFlush(). On other platforms: no-op (frame budget sleep handles pacing).
pub fn sync_to_display() {
    #[cfg(target_os = "windows")]
    windows::sync_to_display();
}
