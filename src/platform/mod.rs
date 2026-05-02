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
        // On Linux, refresh rate could be read via xrandr or DRM, but that requires
        // additional dependencies. 60 Hz is a safe default for now.
        60
    }
}
