/// Wayland overlay setup stub.
///
/// Full wlr-layer-shell support would require `zwlr_layer_shell_v1` protocol
/// negotiation at surface creation time, which winit 0.30 does not yet expose
/// via a stable public API. This module is a placeholder that logs a warning
/// and lets the window fall back to a regular toplevel window.
pub fn setup_wayland_overlay() {
    log::warn!(
        "Wayland overlay: wlr-layer-shell not yet integrated — window will appear as a regular toplevel. \
         Click-through and always-on-top behaviour may be limited."
    );
}
