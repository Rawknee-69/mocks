/// Win32 overlay window setup: transparent, click-through, always-on-top.
///
/// Transparency is achieved via UpdateLayeredWindow (see overlay/windows_impl.rs).
/// Requirements:
///   1. WS_EX_LAYERED must be SET. The UpdateLayeredWindow API requires it.
///   2. SetLayeredWindowAttributes must NOT be called. Calling it switches the
///      window into legacy colorkey/opacity mode, which disables UpdateLayeredWindow.
///   3. WS_EX_TRANSPARENT routes mouse hit-tests to the window below (click-through).
///   4. DwmExtendFrameIntoClientArea is still called so DWM draws a fully-blended
///      glass backdrop behind transparent pixels (cosmetically nicer on Win11).
#[cfg(target_os = "windows")]
pub fn setup_windows_overlay(hwnd: isize) {
    use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::Controls::MARGINS;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        GWL_EXSTYLE, SW_SHOWNOACTIVATE,
        WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };

    const DWMWA_DISALLOW_PEEK: u32 = 11;
    const DWMWA_EXCLUDED_FROM_PEEK: u32 = 12;

    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;

    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;

        let updated = (current
            | WS_EX_LAYERED       // required for UpdateLayeredWindow
            | WS_EX_TRANSPARENT   // click-through: hit-tests fall to the window behind
            | WS_EX_TOOLWINDOW    // hide from taskbar
            | WS_EX_NOACTIVATE)   // never steal focus
            & !WS_EX_APPWINDOW;   // hide from Alt+Tab
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, updated as isize);

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0, 0, 0, 0,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // DWM glass over the entire client area. This keeps the DWM redirection
        // surface alive and composites through the surface alpha channel. Must be
        // called AFTER SetWindowLongPtrW so the layered attribute is already set.
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);

        let one: u32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_DISALLOW_PEEK,
            std::ptr::addr_of!(one).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_EXCLUDED_FROM_PEEK,
            std::ptr::addr_of!(one).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    }

    log::info!("Windows overlay: WS_EX_LAYERED | WS_EX_TRANSPARENT applied, DWM glass extended");
}

#[cfg(not(target_os = "windows"))]
pub fn setup_windows_overlay(_hwnd: isize) {}
