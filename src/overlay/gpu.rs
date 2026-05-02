/// GPU/presentation renderer.
///
/// On Windows we bypass the GPU swap chain entirely and use Win32 UpdateLayeredWindow
/// with per-pixel alpha. This is the only approach that works reliably on all Intel,
/// AMD, and Nvidia drivers on Windows -- neither DX12 nor Vulkan expose premultiplied
/// alpha for a plain HWND swap chain on Intel Iris Xe / many integrated GPUs.
///
/// On other platforms we keep the wgpu pipeline (Vulkan/Metal surface with PreMultiplied
/// or PostMultiplied alpha).
#[cfg(target_os = "windows")]
pub use crate::overlay::windows_impl::Renderer;

#[cfg(not(target_os = "windows"))]
pub use crate::overlay::wgpu_impl::Renderer;
