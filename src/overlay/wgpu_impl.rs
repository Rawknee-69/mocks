/// wgpu-based renderer for non-Windows platforms (Linux/macOS).
/// On these platforms, Vulkan/Metal surfaces expose PreMultiplied alpha.
use bytemuck::{Pod, Zeroable};
use cosmic_text::{FontSystem, SwashCache};
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::overlay::renderer::OverlayRenderState;

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    tex_coords: [f32; 2],
}

const VERTICES: &[Vertex] = &[
    Vertex { position: [-1.0,  1.0], tex_coords: [0.0, 0.0] },
    Vertex { position: [-1.0, -1.0], tex_coords: [0.0, 1.0] },
    Vertex { position: [ 1.0, -1.0], tex_coords: [1.0, 1.0] },
    Vertex { position: [-1.0,  1.0], tex_coords: [0.0, 0.0] },
    Vertex { position: [ 1.0, -1.0], tex_coords: [1.0, 1.0] },
    Vertex { position: [ 1.0,  1.0], tex_coords: [1.0, 0.0] },
];

const WGSL_SHADER: &str = r#"
struct Vertex { @location(0) position: vec2<f32>, @location(1) tex_coords: vec2<f32> };
struct VOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(v: Vertex) -> VOut {
    var o: VOut; o.clip = vec4<f32>(v.position, 0.0, 1.0); o.uv = v.tex_coords; return o;
}
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@fragment fn fs(v: VOut) -> @location(0) vec4<f32> { return textureSample(t, s, v.uv); }
"#;

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    vertex_buffer: wgpu::Buffer,
    overlay_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    surface_config: wgpu::SurfaceConfiguration,
    pixmap: tiny_skia::Pixmap,
    pub swash_cache: SwashCache,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance.create_surface(window).expect("Failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .or_else(|| pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            compatible_surface: Some(&surface),
            force_fallback_adapter: true,
        })))
        .expect("No wgpu adapter found");

        log::info!("wgpu adapter: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);

        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default(), None)
        ).expect("Failed to request wgpu device");

        let capabilities = surface.get_capabilities(&adapter);
        log::info!("wgpu alpha modes: {:?}", capabilities.alpha_modes);

        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
            wgpu::CompositeAlphaMode::Opaque,
        ]
        .into_iter()
        .find(|m| capabilities.alpha_modes.contains(m))
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);

        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::AutoVsync, wgpu::PresentMode::Fifo]
            .into_iter().find(|m| capabilities.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::AutoVsync);

        let surface_format = capabilities.formats[0];

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width, height, present_mode, alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        log::info!("wgpu surface: {:?} {}x{} alpha={:?}", surface_format, width, height, alpha_mode);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_shader"),
            source: wgpu::ShaderSource::Wgsl(WGSL_SHADER.into()),
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { multisampled: false, view_dimension: wgpu::TextureViewDimension::D2, sample_type: wgpu::TextureSampleType::Float { filterable: true } },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay_pipeline"),
            layout: Some(&device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None, bind_group_layouts: &[&bgl], push_constant_ranges: &[],
            })),
            vertex: wgpu::VertexState {
                module: &shader, entry_point: "vs",
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                        wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader, entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
                        alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vb"), contents: bytemuck::cast_slice(VERTICES), usage: wgpu::BufferUsages::VERTEX,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pixmap = tiny_skia::Pixmap::new(width, height).expect("pixmap");
        let (overlay_texture, bind_group) = make_texture_bg(&device, &bgl, &sampler, width, height);

        Self { surface, device, queue, pipeline, bind_group_layout: bgl, vertex_buffer, overlay_texture, bind_group, sampler, surface_config, pixmap, swash_cache: SwashCache::new() }
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        let w = new_size.width.max(1);
        let h = new_size.height.max(1);
        self.surface_config.width = w;
        self.surface_config.height = h;
        self.surface.configure(&self.device, &self.surface_config);
        let (t, bg) = make_texture_bg(&self.device, &self.bind_group_layout, &self.sampler, w, h);
        self.overlay_texture = t;
        self.bind_group = bg;
        self.pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
    }

    pub fn render_frame(&mut self, render_state: &mut OverlayRenderState, font_system: &mut FontSystem, elapsed_secs: f64) {
        crate::overlay::paint::draw_overlay_frame(&mut self.pixmap, render_state, font_system, &mut self.swash_cache, elapsed_secs);

        self.queue.write_texture(
            wgpu::ImageCopyTexture { texture: &self.overlay_texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            self.pixmap.data(),
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(self.pixmap.width() * 4), rows_per_image: Some(self.pixmap.height()) },
            wgpu::Extent3d { width: self.pixmap.width(), height: self.pixmap.height(), depth_or_array_layers: 1 },
        );

        let st = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(e) => { log::warn!("Surface error: {e:?}"); self.surface.configure(&self.device, &self.surface_config); return; }
        };
        let view = st.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None, timestamp_writes: None, occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.draw(0..VERTICES.len() as u32, 0..1);
        }
        self.queue.submit(std::iter::once(enc.finish()));
        st.present();
    }

    pub fn window_size(&self) -> (u32, u32) {
        (self.surface_config.width, self.surface_config.height)
    }
}

fn make_texture_bg(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, width: u32, height: u32) -> (wgpu::Texture, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("overlay_tex"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("overlay_bg"), layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    });
    (texture, bg)
}
