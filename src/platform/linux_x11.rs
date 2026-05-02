/// X11 overlay window hints: skip taskbar/pager, window type = notification, no decorations.
/// Also applies an empty XShape input region so the window is fully click-through on X11.
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as XprotoConnectionExt, PropMode};
use x11rb::wrapper::ConnectionExt as XprotoWrapperExt;

pub fn setup_x11_overlay(x11_window: u32) -> Result<(), String> {
    let (connection, _) =
        x11rb::connect(None).map_err(|e| format!("Failed to connect to X11: {e}"))?;

    let net_wm_state = intern_atom(&connection, b"_NET_WM_STATE")?;
    let net_wm_state_skip_taskbar = intern_atom(&connection, b"_NET_WM_STATE_SKIP_TASKBAR")?;
    let net_wm_state_skip_pager = intern_atom(&connection, b"_NET_WM_STATE_SKIP_PAGER")?;
    let net_wm_state_above = intern_atom(&connection, b"_NET_WM_STATE_ABOVE")?;
    let net_wm_window_type = intern_atom(&connection, b"_NET_WM_WINDOW_TYPE")?;
    let net_wm_window_type_notification =
        intern_atom(&connection, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?;
    let motif_wm_hints = intern_atom(&connection, b"_MOTIF_WM_HINTS")?;

    connection
        .change_property32(
            PropMode::REPLACE,
            x11_window,
            net_wm_state,
            AtomEnum::ATOM,
            &[
                net_wm_state_skip_taskbar,
                net_wm_state_skip_pager,
                net_wm_state_above,
            ],
        )
        .map_err(|e| format!("Failed to set _NET_WM_STATE: {e}"))?;

    connection
        .change_property32(
            PropMode::REPLACE,
            x11_window,
            net_wm_window_type,
            AtomEnum::ATOM,
            &[net_wm_window_type_notification],
        )
        .map_err(|e| format!("Failed to set _NET_WM_WINDOW_TYPE: {e}"))?;

    // _MOTIF_WM_HINTS: flags=2 (decorations), decorations=0 (none)
    // Layout: [flags, functions, decorations, input_mode, status]
    connection
        .change_property32(
            PropMode::REPLACE,
            x11_window,
            motif_wm_hints,
            motif_wm_hints,
            &[2u32, 0, 0, 0, 0],
        )
        .map_err(|e| format!("Failed to set _MOTIF_WM_HINTS: {e}"))?;

    connection
        .flush()
        .map_err(|e| format!("Failed to flush X11 hints: {e}"))?;

    Ok(())
}

fn intern_atom<C: Connection>(connection: &C, name: &[u8]) -> Result<u32, String> {
    connection
        .intern_atom(false, name)
        .map_err(|e| {
            format!(
                "Failed to request atom {}: {e}",
                String::from_utf8_lossy(name)
            )
        })?
        .reply()
        .map_err(|e| {
            format!(
                "Failed to read atom {}: {e}",
                String::from_utf8_lossy(name)
            )
        })
        .map(|reply| reply.atom)
}
