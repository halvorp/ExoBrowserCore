//! Minimal wgpu renderer for RAW_FRAME messages.
//!
//! Owns a window, a BGRA texture sized to the incoming frame, and a
//! fullscreen quad that samples it. Frames arrive over an mpsc that
//! `run()` polls between winit events; each new frame reuploads the
//! texture and requests a redraw.

use anyhow::{Context, Result};
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes};

/// One frame handed off from the network thread to the render thread.
pub struct GfxFrame {
    pub width:  u32,
    pub height: u32,
    pub stride: u32,       // bytes per row (may be > width*4)
    pub pixels: Vec<u8>,   // BGRA (wl_shm ARGB8888 in memory)
}

/// A sub-rect update — blit at (x, y) inside the existing video texture.
pub struct GfxSubframe {
    pub x: u32, pub y: u32,
    pub w: u32, pub h: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
}

/// Height of the URL bar drawn at the top of the window.
pub const URL_BAR_H: u32 = 24;
/// Each font glyph pixel becomes SCALE×SCALE screen pixels. (Matches
/// the `scale` constant hard-coded in the WGSL shader below.)
#[allow(dead_code)]
const URL_TEXT_SCALE: u32 = 2;
/// Bar's horizontal padding in screen pixels. (Matches WGSL literal.)
#[allow(dead_code)]
const URL_BAR_PAD_X: u32 = 8;
/// Max URL chars we render (packed into uniform, so bounded).
const URL_MAX_CHARS: usize = 32;
/// Font atlas dimensions: 128 chars side-by-side, 8 rows tall.
const FONT_ATLAS_W: u32 = 128 * 8;
const FONT_ATLAS_H: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex { pos: [f32; 2], uv: [f32; 2] }

// Fullscreen quad (two triangles). UV top-left origin = matches BGRA row-0 up.
const QUAD: &[Vertex] = &[
    Vertex { pos: [-1.0, -1.0], uv: [0.0, 1.0] },
    Vertex { pos: [ 1.0, -1.0], uv: [1.0, 1.0] },
    Vertex { pos: [-1.0,  1.0], uv: [0.0, 0.0] },
    Vertex { pos: [-1.0,  1.0], uv: [0.0, 0.0] },
    Vertex { pos: [ 1.0, -1.0], uv: [1.0, 1.0] },
    Vertex { pos: [ 1.0,  1.0], uv: [1.0, 0.0] },
];

const SHADER_WGSL: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
struct Uni  {
    cursor:   vec2<f32>,
    viewport: vec2<f32>,
    scroll:   vec2<f32>,
    // shape.x = cursor shape id (0 arrow, 1 pointer, 2 text)
    // shape.y = URL edit mode (0 hidden, 1 idle, 2 focused/editing)
    shape:    vec2<f32>,
    // 32 URL chars packed as 8 × vec4<u32> = 8 * 4 = 32 u32 codepoints.
    url:      array<vec4<u32>, 8>,
};

@vertex fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VOut {
    var o: VOut;
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    o.uv  = uv;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> u: Uni;
@group(0) @binding(3) var font_tex: texture_2d<f32>;

fn url_char_at(i: i32) -> u32 {
    let v = u.url[i / 4];
    let lane = i % 4;
    if (lane == 0) { return v.x; }
    if (lane == 1) { return v.y; }
    if (lane == 2) { return v.z; }
    return v.w;
}

@fragment fn fs(in: VOut) -> @location(0) vec4<f32> {
    let px_y = in.uv.y * u.viewport.y;
    let px_x = in.uv.x * u.viewport.x;
    let bar_mode = i32(u.shape.y);
    if (bar_mode > 0 && px_y < 24.0) {
        // Background color per mode.
        var bg = vec4<f32>(0.10, 0.10, 0.12, 1.0);
        var sep = vec4<f32>(0.30, 0.30, 0.34, 1.0);
        if (bar_mode == 2) {
            bg  = vec4<f32>(0.12, 0.14, 0.20, 1.0);
            sep = vec4<f32>(0.30, 0.75, 1.00, 1.0);
        } else if (bar_mode == 3) {
            // Loading: warm amber bar so you know the server is working.
            bg  = vec4<f32>(0.28, 0.15, 0.03, 1.0);
            sep = vec4<f32>(1.00, 0.55, 0.10, 1.0);
        }
        // Bottom separator line.
        if (px_y >= 23.0) { return sep; }

        // Text sampling. Each glyph is 8×8, scaled by URL_TEXT_SCALE (=2)
        // to 16×16 screen px. Text baseline: bar top + 4px padding.
        let scale = 2.0;
        let text_y0 = 4.0;
        let text_x0 = 8.0; // URL_BAR_PAD_X
        let idx = i32(floor((px_x - text_x0) / (8.0 * scale)));
        if (idx >= 0 && idx < 32 && px_x >= text_x0 && px_y >= text_y0 && px_y < text_y0 + 8.0 * scale) {
            let ch = url_char_at(idx);
            if (ch != 0u && ch < 128u) {
                let gx = i32(floor((px_x - text_x0 - f32(idx) * 8.0 * scale) / scale));
                let gy = i32(floor((px_y - text_y0) / scale));
                let ax = (f32(ch) * 8.0 + f32(gx) + 0.5) / f32(128 * 8);
                let ay = (f32(gy) + 0.5) / 8.0;
                let p  = textureSample(font_tex, samp, vec2<f32>(ax, ay)).r;
                if (p > 0.5) { return vec4<f32>(0.90, 0.90, 0.95, 1.0); }
            }
        }
        // Cursor caret in edit mode: 2-px wide vertical bar right after
        // the last non-zero URL char. Loop finds the length (bounded 32).
        if (bar_mode == 2) {
            var len: i32 = 0;
            for (var i: i32 = 0; i < 32; i = i + 1) {
                if (url_char_at(i) != 0u) { len = i + 1; }
            }
            let caret_x = text_x0 + f32(len) * 8.0 * scale + 1.0;
            if (px_x >= caret_x && px_x < caret_x + 2.0 &&
                px_y >= text_y0 && px_y < text_y0 + 8.0 * scale) {
                return vec4<f32>(0.30, 0.75, 1.00, 1.0);
            }
        }
        return bg;
    }

    // Client-side scroll + video-area remap so the bar doesn't overlap
    // WebKit's content. Video occupies y in [24, viewport.y].
    let video_h = u.viewport.y - 24.0;
    let video_uv = vec2<f32>(in.uv.x, (px_y - 24.0) / video_h);
    let uv_scrolled = video_uv + u.scroll / u.viewport;
    var rgb: vec4<f32>;
    if (uv_scrolled.x < 0.0 || uv_scrolled.x > 1.0 ||
        uv_scrolled.y < 0.0 || uv_scrolled.y > 1.0) {
        rgb = vec4<f32>(0.94, 0.94, 0.94, 1.0);
    } else {
        let t = textureSample(tex, samp, uv_scrolled);
        rgb = vec4<f32>(t.b, t.g, t.r, t.a);
    }

    // Client-side cursor. Shape selected by server hint (u.shape.x):
    //   0 = arrow (default), 1 = pointer/hand (drawn as a small filled disk
    //   with an outline; a real pointing hand needs an SDF), 2 = text I-beam.
    let px = in.uv * u.viewport;
    let d  = px - u.cursor;
    let shape_id = i32(u.shape.x);
    var in_body   = false;
    var in_border = false;
    if (shape_id == 1) {
        // Filled disk r=7 with 1-px outline.
        let r = length(d - vec2<f32>(0.0, 0.0));
        in_body   = r < 6.0;
        in_border = r >= 6.0 && r < 7.5;
    } else if (shape_id == 2) {
        // I-beam: vertical bar height 18, with two 6-wide serifs at ends.
        let ax = abs(d.x);
        let ay = d.y;
        let bar    = ax < 1.5 && ay >= -9.0 && ay <= 9.0;
        let cap_up = ax <= 6.0 && ay >= -10.0 && ay <= -7.0;
        let cap_dn = ax <= 6.0 && ay >=  7.0  && ay <= 10.0;
        in_body   = bar || cap_up || cap_dn;
        in_border = false;
    } else {
        // Arrow (default).
        let inside =
            (d.x >= 0.0 && d.x <= 14.0) &&
            (d.y >= 0.0 && d.y <= 20.0) &&
            (d.y >= d.x * 20.0 / 14.0 - 6.0) &&
            (d.x <= d.y * 0.9);
        let outline =
            inside &&
            !((d.x >= 1.0 && d.x <= 13.0) &&
              (d.y >= 1.0 && d.y <= 19.0) &&
              (d.y >= (d.x - 1.0) * 20.0 / 14.0 - 5.0) &&
              (d.x <= (d.y - 1.0) * 0.9));
        in_body   = inside && !outline;
        in_border = outline;
    }
    if (in_border) { rgb = vec4<f32>(0.0, 0.0, 0.0, 1.0); }
    else if (in_body) { rgb = vec4<f32>(1.0, 1.0, 1.0, 1.0); }
    return rgb;
}
"#;

#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CursorUniform {
    cursor:   [f32; 2],
    viewport: [f32; 2],
    scroll:   [f32; 2],
    shape:    [f32; 2],
    /// 32 URL char codepoints packed as 8 × [u32; 4] (matches WGSL vec4<u32>[8]).
    url:      [[u32; 4]; 8],
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device:  wgpu::Device,
    queue:   wgpu::Queue,
    config:  wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    quad_vb: wgpu::Buffer,
    tex:     Option<wgpu::Texture>,
    tex_size: (u32, u32),
    bind_group: Option<wgpu::BindGroup>,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cursor_uniform: wgpu::Buffer,
    cursor_data:    CursorUniform,
    tile_cache:     std::collections::HashMap<u64, Vec<u8>>,
    _font_tex:      wgpu::Texture,
    _font_view:     wgpu::TextureView,
}

impl Gpu {
    async fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).context("create_surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("request_adapter (no compatible GPU)")?;
        let info = adapter.get_info();
        tracing::info!(backend = ?info.backend, adapter = %info.name, "wgpu adapter");

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("gutted-client"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                },
                None,
            )
            .await?;

        let size = window.inner_size();
        let surf_caps = surface.get_capabilities(&adapter);
        let format = surf_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surf_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surf_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-pl"), bind_group_layouts: &[&bind_layout], push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipe"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader, entry_point: "vs", compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader, entry_point: "fs", compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format, blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        let quad_vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-vb"),
            contents: bytemuck::cast_slice(QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let cursor_data = CursorUniform {
            cursor: [-100.0, -100.0],
            viewport: [config.width as f32, config.height as f32],
            scroll: [0.0, 0.0],
            shape: [0.0, 0.0],
            url: [[0; 4]; 8],
        };
        let cursor_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cursor-uniform"),
            contents: bytemuck::bytes_of(&cursor_data),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        // Font atlas — build once from font8x8, upload as R8Unorm.
        let atlas_bytes = build_font_atlas();
        let font_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("font-atlas"),
            size: wgpu::Extent3d { width: FONT_ATLAS_W, height: FONT_ATLAS_H, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &font_tex, mip_level: 0,
                origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All,
            },
            &atlas_bytes,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(FONT_ATLAS_W),
                rows_per_image: Some(FONT_ATLAS_H),
            },
            wgpu::Extent3d { width: FONT_ATLAS_W, height: FONT_ATLAS_H, depth_or_array_layers: 1 },
        );
        let font_view = font_tex.create_view(&wgpu::TextureViewDescriptor::default());

        Ok(Self {
            surface, device, queue, config, pipeline, quad_vb,
            tex: None, tex_size: (0, 0), bind_group: None, bind_layout, sampler,
            cursor_uniform, cursor_data,
            tile_cache: std::collections::HashMap::new(),
            _font_tex: font_tex, _font_view: font_view,
        })
    }

    /// Pack a URL string into the uniform. Truncates to URL_MAX_CHARS.
    fn set_url_text(&mut self, url: &str) {
        let mut chars = [[0u32; 4]; 8];
        for (i, c) in url.chars().take(URL_MAX_CHARS).enumerate() {
            let v = c as u32;
            let v = if v < 128 { v } else { b'?' as u32 };
            chars[i / 4][i % 4] = v;
        }
        self.cursor_data.url = chars;
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    fn set_cursor(&mut self, x: f32, y: f32) {
        self.cursor_data.cursor = [x, y];
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    fn set_scroll(&mut self, sx: f32, sy: f32) {
        self.cursor_data.scroll = [sx, sy];
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    fn set_shape(&mut self, shape: u8) {
        self.cursor_data.shape[0] = shape as f32;
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    /// URL bar mode: 0 hidden, 1 idle, 2 focused.
    fn set_url_bar(&mut self, mode: u8) {
        self.cursor_data.shape[1] = mode as f32;
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    /// Blit a sub-rect into the existing frame texture. Warns (not
    /// silent!) if the texture doesn't exist yet — that used to hide
    /// the "black screen" bug where the first frame off the wire was a
    /// SUBFRAME and there was no base to overlay onto.
    fn upload_sub(&mut self, s: &GfxSubframe) {
        let Some(tex) = self.tex.as_ref() else {
            tracing::warn!(x = s.x, y = s.y, w = s.w, h = s.h,
                "SUBFRAME arrived before any RawFrame — no base texture; dropping");
            return;
        };
        // Silent clamp: if the sub-rect exceeds the current tex, skip.
        if s.x + s.w > self.tex_size.0 || s.y + s.h > self.tex_size.1 { return; }
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: tex, mip_level: 0,
                origin: wgpu::Origin3d { x: s.x, y: s.y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &s.pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(s.stride),
                rows_per_image: Some(s.h),
            },
            wgpu::Extent3d { width: s.w, height: s.h, depth_or_array_layers: 1 },
        );
    }

    fn upload(&mut self, f: &GfxFrame) {
        let (w, h) = (f.width, f.height);
        if self.tex.is_none() || self.tex_size != (w, h) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("wpe-frame"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1, sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blit-bg"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: self.cursor_uniform.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&self._font_view) },
                ],
            });
            self.tex = Some(tex);
            self.tex_size = (w, h);
            self.bind_group = Some(bg);
        }
        let tex = self.tex.as_ref().unwrap();
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: tex, mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &f.pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(f.stride),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    fn resize(&mut self, w: u32, h: u32) {
        self.config.width  = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
        self.cursor_data.viewport = [self.config.width as f32, self.config.height as f32];
        self.queue.write_buffer(&self.cursor_uniform, 0, bytemuck::bytes_of(&self.cursor_data));
    }

    /// Read back the current surface texture as an RGBA8 buffer of size
    /// (config.width, config.height). Slow — for debug/screenshot only.
    fn capture(&self) -> Result<(u32, u32, Vec<u8>)> {
        let w = self.config.width;
        let h = self.config.height;
        // wgpu requires bytes_per_row to be a multiple of 256.
        let unpadded = (w * 4) as usize;
        let padded   = (unpadded + 255) & !255;
        let size = (padded * h as usize) as u64;
        let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture-tex"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("capture-rp"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None, timestamp_writes: None, occlusion_query_set: None,
            });
            if let Some(bg) = self.bind_group.as_ref() {
                rp.set_pipeline(&self.pipeline);
                rp.set_bind_group(0, bg, &[]);
                rp.set_vertex_buffer(0, self.quad_vb.slice(..));
                rp.draw(0..6, 0..1);
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex, mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &out_buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0, bytes_per_row: Some(padded as u32), rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = out_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { tx.send(r).ok(); });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap()?;
        let data = slice.get_mapped_range();
        // Compact away the row padding.
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            let start = y * padded;
            out.extend_from_slice(&data[start..start + unpadded]);
        }
        Ok((w, h, out))
    }

    fn render(&mut self) -> Result<()> {
        let Some(bg) = self.bind_group.as_ref() else {
            // No frame yet — clear and skip.
            let out = self.surface.get_current_texture()?;
            let view = out.texture.create_view(&Default::default());
            let mut enc = self.device.create_command_encoder(&Default::default());
            {
                let _rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view, resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.02, g: 0.02, b: 0.04, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
            }
            self.queue.submit([enc.finish()]);
            out.present();
            return Ok(());
        };

        let out = self.surface.get_current_texture()?;
        let view = out.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, bg, &[]);
            rp.set_vertex_buffer(0, self.quad_vb.slice(..));
            rp.draw(0..6, 0..1);
        }
        self.queue.submit([enc.finish()]);
        out.present();
        Ok(())
    }
}

pub enum RenderEvent {
    Frame(GfxFrame),
    Subframe(GfxSubframe),
    TileData { hash: u64, pixels: Vec<u8> },
    TileRef  { x: u32, y: u32, w: u32, h: u32, hash: u64 },
    /// Server-hinted cursor shape (0 = default, 1 = pointer, 2 = text).
    CursorShape(u8),
    /// Server-side load state: 0 started, 1 redirected, 2 committed, 3 finished.
    LoadState(u8),
    /// URL the server was already on when we connected.
    InitialUrl(String),
    /// WebKit page <title> changed — client updates window chrome.
    Title(String),
    /// WebKit committed URL changed — client refreshes URL bar
    /// unless the user is editing.
    UrlChanged(String),
    Quit,
}

/// Input event emitted from the winit thread, consumed by the net thread.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Motion { x: i32, y: i32, mods: u32 },
    Button { x: i32, y: i32, button: u32, pressed: bool, mods: u32 },
    Key    { keysym: u32, mods: u32, pressed: bool },
    /// Wheel notch delta (positive dy = scroll down / content up).
    Scroll { dx: i32, dy: i32 },
    /// Client-initiated navigation. Not forwarded to WebKit as input —
    /// net thread routes to the ctrl stream as a Nav message.
    Navigate(String),
    /// Viewport size changed (excludes the URL bar area). Net thread
    /// routes to ctrl as Resize; server calls wpe_view_backend_dispatch_set_size.
    Resize { w: u16, h: u16 },
    /// Set WebKit zoom level. 1000 = 100%; host clamps.
    SetZoom { level_milli: u32 },
    /// History nav. 0=back, 1=forward, 2=reload.
    NavAction { action: u8 },
}

/// Prepend `https://` if the user typed a bare domain. Passes through
/// anything with an explicit scheme; empty → about:blank.
fn canonicalize_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() { return "about:blank".into(); }
    if let Some(colon) = s.find(':') {
        let scheme_ok = colon > 0
            && s[..colon].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && s[..colon].chars().next().map_or(false, |c| c.is_ascii_alphabetic());
        if scheme_ok { return s.into(); }
    }
    format!("https://{s}")
}

#[cfg(test)]
mod url_tests {
    use super::canonicalize_url;
    #[test]
    fn adds_https() { assert_eq!(canonicalize_url("example.com"), "https://example.com"); }
    #[test]
    fn keeps_scheme() { assert_eq!(canonicalize_url("http://x"), "http://x"); }
    #[test]
    fn keeps_data() { assert_eq!(canonicalize_url("data:text/html,x"), "data:text/html,x"); }
    #[test]
    fn keeps_about() { assert_eq!(canonicalize_url("about:blank"), "about:blank"); }
    #[test]
    fn empty_is_blank() { assert_eq!(canonicalize_url(""), "about:blank"); }
    #[test]
    fn trims_spaces() { assert_eq!(canonicalize_url("  foo.com  "), "https://foo.com"); }
    #[test]
    fn colon_but_not_scheme() { assert_eq!(canonicalize_url("1:2"), "https://1:2"); }
}

/// F1-F9 bookmark URLs. F5 is reserved for reload (browser convention).
/// Kept in the client because F-keys are typically consumed by the UI,
/// not the page. Trivially extended.
pub const BOOKMARKS: &[(&str, &str)] = &[
    ("F1", "https://example.com"),
    ("F2", "https://www.wikipedia.org"),
    ("F3", "https://news.ycombinator.com"),
    ("F4", "about:blank"),
    // F5 = reload (handled specially in the key handler).
    ("F5", ""),
    ("F6", "https://startpage.com"),
    ("F7", "https://duckduckgo.com"),
    ("F8", "https://en.wikipedia.org/wiki/QUIC"),
    ("F9", "https://webkit.org"),
];

struct App {
    window: Option<Arc<Window>>,
    gpu:    Option<Gpu>,
    frames: Option<std::sync::mpsc::Receiver<RenderEvent>>,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
    cursor: (i32, i32),
    mods:   u32,
    /// Accumulated pixel scroll offset — client applies this locally, server
    /// catches up asynchronously. Reset to (0,0) whenever a new frame arrives
    /// (naive reconciliation: assume server rendered the scrolled state).
    scroll_px: (f32, f32),
    /// URL bar state. `editing`=true when accepting key input into `url_buf`.
    url_buf: String,
    editing: bool,
    capture_to: Option<std::path::PathBuf>,
    capture_after_frames: u32,
    frames_seen: u32,
    synth_cursor: Option<(i32, i32)>,
    /// For headless visual tests: warp scroll uniform after upload, once.
    synth_scroll: Option<(f32, f32)>,
    /// For headless URL bar tests: prefill the URL text at first frame.
    synth_url: Option<String>,
    /// Current page zoom, in per-mille (1000 = 100%).
    zoom_milli: u32,
}

// WPE modifier bit positions (mirror of wpe/input.h enum wpe_input_modifier).
const MOD_CTRL:    u32 = 1 << 0;
const MOD_SHIFT:   u32 = 1 << 1;
const MOD_ALT:     u32 = 1 << 2;
const MOD_META:    u32 = 1 << 3;
const MOD_BUTTON1: u32 = 1 << 20;
const MOD_BUTTON2: u32 = 1 << 21;
const MOD_BUTTON3: u32 = 1 << 22;

impl ApplicationHandler<RenderEvent> for App {
    fn resumed(&mut self, elwt: &ActiveEventLoop) {
        if self.window.is_some() { return; }
        let win = elwt.create_window(
            WindowAttributes::default()
                .with_title("gutted-client")
                .with_inner_size(winit::dpi::PhysicalSize::new(1280u32, 720u32))
        ).expect("create_window");
        win.set_cursor_visible(false);
        let sz = win.inner_size();
        let win = Arc::new(win);
        let mut gpu = pollster::block_on(Gpu::new(win.clone())).expect("Gpu::new");
        gpu.set_url_bar(1); // idle bar visible by default
        // Send the initial viewport to the server — winit's Resized doesn't
        // always fire on first show, so we bootstrap explicitly.
        let vh = sz.height.saturating_sub(URL_BAR_H);
        if vh > 0 && sz.width > 0 {
            let _ = self.input_tx.send(InputEvent::Resize {
                w: sz.width as u16, h: vh as u16,
            });
        }
        self.window = Some(win);
        self.gpu = Some(gpu);
    }

    fn user_event(&mut self, elwt: &ActiveEventLoop, ev: RenderEvent) {
        match ev {
            RenderEvent::Frame(f) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.upload(&f);
                    if let Some((sx, sy)) = self.synth_cursor.take() {
                        gpu.set_cursor(sx as f32, sy as f32);
                    }
                    // Reconcile any real scroll offset (server just caught up).
                    if self.scroll_px != (0.0, 0.0) {
                        self.scroll_px = (0.0, 0.0);
                        gpu.set_scroll(0.0, 0.0);
                    }
                    // Apply synth_scroll AFTER reconcile so headless tests see the shift.
                    if let Some((sx, sy)) = self.synth_scroll.take() {
                        gpu.set_scroll(sx, sy);
                    }
                    if let Some(u) = self.synth_url.take() {
                        gpu.set_url_bar(2);
                        gpu.set_url_text(&u);
                    }
                    self.frames_seen += 1;
                    if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                }
            }
            RenderEvent::TileData { hash, pixels } => {
                if let Some(g) = self.gpu.as_mut() {
                    g.tile_cache.insert(hash, pixels);
                }
            }
            RenderEvent::TileRef { x, y, w, h, hash } => {
                if let Some(g) = self.gpu.as_mut() {
                    if let Some(pixels) = g.tile_cache.get(&hash) {
                        g.upload_sub(&GfxSubframe {
                            x, y, w, h, stride: w * 4, pixels: pixels.clone(),
                        });
                        self.frames_seen += 1;
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                }
            }
            RenderEvent::Subframe(s) => {
                if let Some(g) = self.gpu.as_mut() {
                    g.upload_sub(&s);
                    self.frames_seen += 1;
                }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
            }
            RenderEvent::CursorShape(sid) => {
                if let Some(g) = self.gpu.as_mut() { g.set_shape(sid); }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
            }
            RenderEvent::LoadState(s) => {
                let mode: u8 = if s < 3 { 3 } else if self.editing { 2 } else { 1 };
                if let Some(g) = self.gpu.as_mut() { g.set_url_bar(mode); }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
            }
            RenderEvent::InitialUrl(u) => {
                self.url_buf = u.clone();
                if let Some(g) = self.gpu.as_mut() { g.set_url_text(&u); }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
            }
            RenderEvent::Title(t) => {
                if let Some(w) = self.window.as_ref() {
                    let full = if t.is_empty() { "gutted-client".into() }
                               else { format!("{t} — gutted-client") };
                    w.set_title(&full);
                }
            }
            RenderEvent::UrlChanged(u) => {
                if !self.editing {
                    self.url_buf = u.clone();
                    if let Some(g) = self.gpu.as_mut() { g.set_url_text(&u); }
                }
                // New page → server-side scroll reset; drop local overlay so
                // we don't render a shifted "wrong" view for one round-trip.
                if self.scroll_px != (0.0, 0.0) {
                    self.scroll_px = (0.0, 0.0);
                    if let Some(g) = self.gpu.as_mut() { g.set_scroll(0.0, 0.0); }
                }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
            }
            RenderEvent::Quit => elwt.exit(),
        }
        self.maybe_capture(elwt);
    }

    fn window_event(&mut self, elwt: &ActiveEventLoop, _id: winit::window::WindowId, ev: WindowEvent) {
        use winit::event::{ElementState, MouseButton};
        use winit::keyboard::{ModifiersState, PhysicalKey};
        match ev {
            WindowEvent::CloseRequested => elwt.exit(),
            WindowEvent::Resized(sz) => {
                if let Some(g) = self.gpu.as_mut() { g.resize(sz.width, sz.height); }
                // Subtract URL bar height so the server renders content that
                // matches the visible video area (not the whole window).
                let vh = sz.height.saturating_sub(URL_BAR_H);
                if vh > 0 && sz.width > 0 {
                    let _ = self.input_tx.send(InputEvent::Resize {
                        w: sz.width as u16, h: vh as u16,
                    });
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(g) = self.gpu.as_mut() { let _ = g.render(); }
                self.frames_seen += 1;
                self.maybe_capture(elwt);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as i32, position.y as i32);
                // Local cursor draws at raw screen position (bar overlay too).
                if let Some(g) = self.gpu.as_mut() {
                    g.set_cursor(position.x as f32, position.y as f32);
                }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                // Forward WebKit-relative y (subtract bar height). Motion in
                // the bar area sends y=0 so WPE doesn't see negative coords.
                let webkit_y = (self.cursor.1 - URL_BAR_H as i32).max(0);
                let _ = self.input_tx.send(InputEvent::Motion {
                    x: self.cursor.0, y: webkit_y, mods: self.mods,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Mouse Back/Forward → history nav (on press only).
                if matches!(state, ElementState::Pressed) {
                    match button {
                        MouseButton::Back => {
                            let _ = self.input_tx.send(InputEvent::NavAction { action: 0 });
                            return;
                        }
                        MouseButton::Forward => {
                            let _ = self.input_tx.send(InputEvent::NavAction { action: 1 });
                            return;
                        }
                        _ => {}
                    }
                }
                let (btn, mask) = match button {
                    MouseButton::Left   => (1, MOD_BUTTON1),
                    MouseButton::Right  => (3, MOD_BUTTON3),
                    MouseButton::Middle => (2, MOD_BUTTON2),
                    _                   => (0, 0),
                };
                if btn == 0 { return; }
                let pressed = matches!(state, ElementState::Pressed);
                if pressed { self.mods |= mask; } else { self.mods &= !mask; }
                // Clicks in the URL bar don't reach WebKit.
                if (self.cursor.1 as u32) < URL_BAR_H { return; }
                let webkit_y = self.cursor.1 - URL_BAR_H as i32;
                let _ = self.input_tx.send(InputEvent::Button {
                    x: self.cursor.0, y: webkit_y,
                    button: btn, pressed, mods: self.mods,
                });
            }
            WindowEvent::ModifiersChanged(m) => {
                let s: ModifiersState = m.state();
                let mut km = self.mods & (MOD_BUTTON1 | MOD_BUTTON2 | MOD_BUTTON3);
                if s.contains(ModifiersState::CONTROL) { km |= MOD_CTRL; }
                if s.contains(ModifiersState::SHIFT)   { km |= MOD_SHIFT; }
                if s.contains(ModifiersState::ALT)     { km |= MOD_ALT; }
                if s.contains(ModifiersState::SUPER)   { km |= MOD_META; }
                self.mods = km;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                // Convert to notch-ish units. LineDelta already is notches;
                // PixelDelta scales down by 40 px/notch as a heuristic.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p)   => (p.x as f32 / 40.0, p.y as f32 / 40.0),
                };
                // Ctrl+wheel = zoom, not scroll. One notch per 125-milli step.
                if self.mods & MOD_CTRL != 0 {
                    let step: i32 = if dy > 0.0 { 125 } else if dy < 0.0 { -125 } else { 0 };
                    if step != 0 {
                        let next = (self.zoom_milli as i32 + step).clamp(250, 5000) as u32;
                        if next != self.zoom_milli {
                            self.zoom_milli = next;
                            let _ = self.input_tx.send(InputEvent::SetZoom { level_milli: next });
                        }
                    }
                    return;
                }
                // Apply local scroll immediately (positive dy = content moves up).
                self.scroll_px.0 += dx * 40.0;
                self.scroll_px.1 += dy * 40.0;
                if let Some(g) = self.gpu.as_mut() {
                    g.set_scroll(self.scroll_px.0, self.scroll_px.1);
                }
                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                let _ = self.input_tx.send(InputEvent::Scroll {
                    dx: dx.round() as i32, dy: dy.round() as i32,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::KeyCode;
                let pressed = matches!(event.state, ElementState::Pressed);

                // Alt+Left/Right = history back/forward (browser convention).
                if pressed && (self.mods & MOD_ALT != 0) {
                    if let PhysicalKey::Code(kc) = event.physical_key {
                        match kc {
                            KeyCode::ArrowLeft => {
                                let _ = self.input_tx.send(InputEvent::NavAction { action: 0 });
                                return;
                            }
                            KeyCode::ArrowRight => {
                                let _ = self.input_tx.send(InputEvent::NavAction { action: 1 });
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                // F1..F9 → client-side bookmark nav. Consume, don't forward.
                if let PhysicalKey::Code(kc) = event.physical_key {
                    let idx: Option<usize> = match kc {
                        KeyCode::F1 => Some(0), KeyCode::F2 => Some(1),
                        KeyCode::F3 => Some(2), KeyCode::F4 => Some(3),
                        KeyCode::F5 => Some(4), KeyCode::F6 => Some(5),
                        KeyCode::F7 => Some(6), KeyCode::F8 => Some(7),
                        KeyCode::F9 => Some(8),
                        _ => None,
                    };
                    if let Some(i) = idx {
                        if pressed {
                            if i == 4 {
                                // F5 = real WebKit reload (preserves scroll + session).
                                let _ = self.input_tx.send(InputEvent::NavAction { action: 2 });
                            } else if let Some((_, url)) = BOOKMARKS.get(i) {
                                if !url.is_empty() {
                                    if let Some(g) = self.gpu.as_mut() {
                                        g.set_url_bar(1);
                                        g.set_url_text(url);
                                    }
                                    self.url_buf = (*url).into();
                                    if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                                    let _ = self.input_tx.send(InputEvent::Navigate((*url).into()));
                                }
                            }
                        }
                        return;
                    }

                    // Ctrl+0 → reset zoom to 100%.
                    if pressed && kc == KeyCode::Digit0 && (self.mods & MOD_CTRL != 0) {
                        if self.zoom_milli != 1000 {
                            self.zoom_milli = 1000;
                            let _ = self.input_tx.send(InputEvent::SetZoom { level_milli: 1000 });
                        }
                        return;
                    }
                    // Ctrl+= / Ctrl+Plus / Ctrl+- → zoom in/out one step.
                    // Equal is where + lives on US layouts; NumpadAdd/Subtract too.
                    if pressed && (self.mods & MOD_CTRL != 0) {
                        let step: i32 = match kc {
                            KeyCode::Equal | KeyCode::NumpadAdd      =>  125,
                            KeyCode::Minus | KeyCode::NumpadSubtract => -125,
                            _ => 0,
                        };
                        if step != 0 {
                            let next = (self.zoom_milli as i32 + step).clamp(250, 5000) as u32;
                            if next != self.zoom_milli {
                                self.zoom_milli = next;
                                let _ = self.input_tx.send(InputEvent::SetZoom { level_milli: next });
                            }
                            return;
                        }
                    }

                    // Ctrl+L → focus URL bar for editing.
                    if pressed && kc == KeyCode::KeyL && (self.mods & MOD_CTRL != 0) {
                        self.editing = true;
                        self.url_buf.clear();
                        if let Some(g) = self.gpu.as_mut() {
                            g.set_url_bar(2);
                            g.set_url_text("");
                        }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                        eprintln!("[url] edit mode ON");
                        return;
                    }

                    if self.editing && pressed {
                        match kc {
                            KeyCode::Escape => {
                                self.editing = false; self.url_buf.clear();
                                if let Some(g) = self.gpu.as_mut() {
                                    g.set_url_bar(1);
                                    g.set_url_text("");
                                }
                                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                                eprintln!("[url] cancelled");
                                return;
                            }
                            KeyCode::Enter | KeyCode::NumpadEnter => {
                                let raw = std::mem::take(&mut self.url_buf);
                                let url = canonicalize_url(&raw);
                                self.editing = false;
                                if let Some(g) = self.gpu.as_mut() {
                                    g.set_url_bar(1);
                                    g.set_url_text(&url);
                                }
                                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                                eprintln!("[url] navigate: {url}");
                                let _ = self.input_tx.send(InputEvent::Navigate(url));
                                return;
                            }
                            KeyCode::Backspace => {
                                self.url_buf.pop();
                                if let Some(g) = self.gpu.as_mut() { g.set_url_text(&self.url_buf); }
                                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                                eprintln!("[url] {}", self.url_buf);
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                // When editing, absorb text from the logical key.
                if self.editing && pressed {
                    if let Some(t) = event.text.as_deref() {
                        for c in t.chars() {
                            if !c.is_control() { self.url_buf.push(c); }
                        }
                        if let Some(g) = self.gpu.as_mut() { g.set_url_text(&self.url_buf); }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                        eprintln!("[url] {}", self.url_buf);
                        return;
                    }
                }

                // Fall through: forward to server.
                let keysym = match event.physical_key {
                    PhysicalKey::Code(kc) => (kc as u32).wrapping_add(0xff00),
                    PhysicalKey::Unidentified(_) => 0,
                };
                if keysym != 0 {
                    let _ = self.input_tx.send(InputEvent::Key {
                        keysym, mods: self.mods, pressed,
                    });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, elwt: &ActiveEventLoop) {
        // Warp the local cursor if a synthetic position was configured
        // (headless test path). Only fires until first CursorMoved event.
        if let (Some((sx, sy)), Some(g)) = (self.synth_cursor.take(), self.gpu.as_mut()) {
            g.set_cursor(sx as f32, sy as f32);
            if let Some(w) = self.window.as_ref() { w.request_redraw(); }
        }
        // Drain any pending frames from the mpsc without blocking so we
        // don't miss quick bursts between winit events.
        if let Some(rx) = self.frames.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    RenderEvent::Frame(f) => {
                        if let Some(g) = self.gpu.as_mut() {
                            g.upload(&f);
                            if let Some((sx, sy)) = self.synth_cursor.take() {
                                g.set_cursor(sx as f32, sy as f32);
                            }
                            if self.scroll_px != (0.0, 0.0) {
                                self.scroll_px = (0.0, 0.0);
                                g.set_scroll(0.0, 0.0);
                            }
                            if let Some((sx, sy)) = self.synth_scroll.take() {
                                g.set_scroll(sx, sy);
                            }
                            if let Some(u) = self.synth_url.take() {
                                g.set_url_bar(2);
                                g.set_url_text(&u);
                            }
                            self.frames_seen += 1;
                        }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::TileData { hash, pixels } => {
                        if let Some(g) = self.gpu.as_mut() {
                            g.tile_cache.insert(hash, pixels);
                        }
                    }
                    RenderEvent::TileRef { x, y, w, h, hash } => {
                        if let Some(g) = self.gpu.as_mut() {
                            if let Some(pixels) = g.tile_cache.get(&hash) {
                                g.upload_sub(&GfxSubframe {
                                    x, y, w, h, stride: w * 4, pixels: pixels.clone(),
                                });
                                self.frames_seen += 1;
                                if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                            }
                        }
                    }
                    RenderEvent::Subframe(s) => {
                        if let Some(g) = self.gpu.as_mut() {
                            g.upload_sub(&s);
                            self.frames_seen += 1;
                        }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::CursorShape(sid) => {
                        if let Some(g) = self.gpu.as_mut() { g.set_shape(sid); }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::LoadState(s) => {
                        let mode: u8 = if s < 3 { 3 } else if self.editing { 2 } else { 1 };
                        if let Some(g) = self.gpu.as_mut() { g.set_url_bar(mode); }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::InitialUrl(u) => {
                        self.url_buf = u.clone();
                        if let Some(g) = self.gpu.as_mut() { g.set_url_text(&u); }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::Title(t) => {
                        if let Some(w) = self.window.as_ref() {
                            let full = if t.is_empty() { "gutted-client".into() }
                                       else { format!("{t} — gutted-client") };
                            w.set_title(&full);
                        }
                    }
                    RenderEvent::UrlChanged(u) => {
                        if !self.editing {
                            self.url_buf = u.clone();
                            if let Some(g) = self.gpu.as_mut() { g.set_url_text(&u); }
                        }
                        if self.scroll_px != (0.0, 0.0) {
                            self.scroll_px = (0.0, 0.0);
                            if let Some(g) = self.gpu.as_mut() { g.set_scroll(0.0, 0.0); }
                        }
                        if let Some(w) = self.window.as_ref() { w.request_redraw(); }
                    }
                    RenderEvent::Quit => { elwt.exit(); return; }
                }
            }
        }
        // Test hook for headless visual verify.
        if let Some(s) = std::env::var("GBROWSER_SYNTH_SHAPE").ok().and_then(|v| v.parse::<u8>().ok()) {
            if let Some(g) = self.gpu.as_mut() { g.set_shape(s); }
            std::env::remove_var("GBROWSER_SYNTH_SHAPE");
            if let Some(w) = self.window.as_ref() { w.request_redraw(); }
        }
        self.maybe_capture(elwt);
    }
}

impl App {
    fn maybe_capture(&mut self, elwt: &ActiveEventLoop) {
        let Some(path) = self.capture_to.clone() else { return; };
        if self.frames_seen < self.capture_after_frames { return; }
        let Some(g) = self.gpu.as_ref() else { return; };
        match g.capture() {
            Ok((w, h, rgba)) => {
                write_ppm_rgba(&path, w, h, &rgba).ok();
                tracing::info!(path = %path.display(), w, h, "captured screenshot");
            }
            Err(e) => tracing::warn!(error = %e, "capture failed"),
        }
        elwt.exit();
    }
}

fn write_ppm_rgba(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    write!(f, "P6\n{w} {h}\n255\n")?;
    // Convert RGBA → RGB and (if surface format is BGRA-ish) swap BGR here.
    // Our shader outputs already-corrected RGB, so no swap needed.
    for chunk in rgba.chunks_exact(4) {
        f.write_all(&[chunk[0], chunk[1], chunk[2]])?;
    }
    Ok(())
}

/// Build the 128-char × 8-row font atlas as R8Unorm bytes.
fn build_font_atlas() -> Vec<u8> {
    use font8x8::UnicodeFonts;
    let mut atlas = vec![0u8; (FONT_ATLAS_W * FONT_ATLAS_H) as usize];
    for code in 0u8..=127 {
        let ch = code as char;
        let glyph = font8x8::BASIC_FONTS.get(ch).unwrap_or([0; 8]);
        for row in 0..8 {
            let bits = glyph[row];
            for col in 0..8 {
                if bits & (1 << col) != 0 {
                    let x = code as u32 * 8 + col as u32;
                    let y = row as u32;
                    atlas[(y * FONT_ATLAS_W + x) as usize] = 255;
                }
            }
        }
    }
    atlas
}

pub fn run(
    frames_rx: std::sync::mpsc::Receiver<RenderEvent>,
    input_tx:  std::sync::mpsc::Sender<InputEvent>,
) -> Result<EventLoopProxy<RenderEvent>> {
    let capture_to = std::env::var("GBROWSER_SCREENSHOT").ok().map(std::path::PathBuf::from);
    let capture_after_frames = std::env::var("GBROWSER_SCREENSHOT_AFTER")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    let synth_cursor = std::env::var("GBROWSER_SYNTH_CURSOR").ok().and_then(|s| {
        let (x, y) = s.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    });
    let synth_scroll = std::env::var("GBROWSER_SYNTH_SCROLL").ok().and_then(|s| {
        let (x, y) = s.split_once(',')?;
        Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
    });
    let synth_url = std::env::var("GBROWSER_SYNTH_URL").ok();

    let event_loop = EventLoop::<RenderEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App {
        window: None, gpu: None,
        frames: Some(frames_rx),
        input_tx,
        cursor: (0, 0), mods: 0,
        scroll_px: (0.0, 0.0),
        url_buf: synth_url.clone().unwrap_or_default(),
        editing: synth_url.is_some(),
        capture_to, capture_after_frames, frames_seen: 0,
        synth_cursor, synth_scroll,
        synth_url,
        zoom_milli: 1000,
    };
    event_loop.run_app(&mut app)?;
    Ok(proxy)
}
