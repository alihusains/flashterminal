//! Terminal Renderer (GPU text rendering)
//!
//! Phase 0.5 deliverable: replaces the colored-quad placeholder with real
//! batched/instanced text rendering on the GPU.
//!
//! Architecture (see ADR 0003):
//!
//! ```text
//! RenderSnapshot (immutable, from TerminalState)
//!         │  + DirtyTracker (consumed per frame)
//!         ▼
//!   per-dirty-row instance encoding          (CPU)
//!         │
//!         ▼
//!   persistent instance buffers              (GPU, partial row updates)
//!         │
//!         ▼
//!   background pass  +  glyph pass           (2 draw calls)
//!         │
//!         ▼
//!   GlyphAtlas (R8, shelf-packed, grows)  ◄── GlyphCache (CPU rasterization)
//! ```
//!
//! Key properties:
//! * The renderer **never touches `TerminalState`** — it receives only the
//!   immutable [`RenderSnapshot`] plus the [`DirtyTracker`].
//! * Instance buffers are sized to the grid and reused across frames; only
//!   dirty rows are rewritten via `queue.write_buffer` with row offsets.
//! * The glyph atlas only uploads *new* glyphs; ordinary typing never
//!   rebuilds it.
//! * No lock is ever taken in the render path: the renderer owns the font
//!   library, glyph cache and atlas exclusively.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use fontdue::layout::GlyphRasterConfig;
use terminal_core::{Attribute, Color, DirtyTracker, RenderSnapshot};
use terminal_text::{FontLibrary, GlyphCache, RasterGlyph};
use wgpu::util::DeviceExt;
use winit::dpi::PhysicalSize;
use winit::window::Window;

// ---------------------------------------------------------------------------
// Cursor style
// ---------------------------------------------------------------------------

/// The shape of the terminal cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStyle {
    #[default]
    Block,
    Beam,
    Underline,
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

pub type Rgba = [f32; 4];

pub const DEFAULT_FG: Rgba = [0.92, 0.92, 0.92, 1.0];
pub const DEFAULT_BG: Rgba = [0.10, 0.10, 0.12, 1.0];
pub const CURSOR_COLOR: Rgba = [0.96, 0.96, 0.96, 1.0];
pub const SELECTION_BG: Rgba = [0.28, 0.42, 0.68, 0.65];
pub const UNDERLINE_COLOR: Rgba = [0.35, 0.65, 1.0, 1.0];

/// The classic xterm ANSI-16 palette.
pub const ANSI_16: [Rgba; 16] = [
    [0.00, 0.00, 0.00, 1.0], // 0 black
    [0.80, 0.16, 0.16, 1.0], // 1 red
    [0.18, 0.70, 0.28, 1.0], // 2 green
    [0.80, 0.62, 0.16, 1.0], // 3 yellow
    [0.27, 0.35, 0.80, 1.0], // 4 blue
    [0.72, 0.28, 0.66, 1.0], // 5 magenta
    [0.20, 0.66, 0.66, 1.0], // 6 cyan
    [0.86, 0.86, 0.86, 1.0], // 7 white
    [0.44, 0.44, 0.44, 1.0], // 8 bright black
    [1.00, 0.36, 0.36, 1.0], // 9 bright red
    [0.36, 0.90, 0.44, 1.0], // 10 bright green
    [1.00, 0.82, 0.36, 1.0], // 11 bright yellow
    [0.45, 0.55, 1.00, 1.0], // 12 bright blue
    [0.92, 0.48, 0.86, 1.0], // 13 bright magenta
    [0.36, 0.86, 0.86, 1.0], // 14 bright cyan
    [1.00, 1.00, 1.00, 1.0], // 15 bright white
];

/// Resolves a terminal color into RGBA.
///
/// `bold` maps ANSI 0-7 to their bright variants (xterm behavior). Pure
/// function so it is unit-testable without a GPU.
pub fn resolve_color(c: Color, default: Rgba, bold: bool) -> Rgba {
    match c {
        Color::Default => default,
        Color::Indexed(i) => {
            let i = i as usize;
            match i {
                0..=15 => {
                    let base = ANSI_16[i];
                    if bold && i < 8 {
                        ANSI_16[i + 8]
                    } else {
                        base
                    }
                }
                16..=231 => {
                    let n = i - 16;
                    let r = n / 36;
                    let g = (n % 36) / 6;
                    let b = n % 6;
                    let v = |x: usize| [0u8, 95, 135, 175, 215, 255][x];
                    [
                        v(r) as f32 / 255.0,
                        v(g) as f32 / 255.0,
                        v(b) as f32 / 255.0,
                        1.0,
                    ]
                }
                232..=255 => {
                    let gray = 8 + 10 * (i - 232);
                    let g = gray as f32 / 255.0;
                    [g, g, g, 1.0]
                }
                _ => default,
            }
        }
        Color::Rgb(r, g, b) => [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0],
    }
}

/// Y position (down from the cell top) of a rasterized glyph's bitmap.
///
/// `ascent` is the baseline's distance down from the cell top. `bearing_y`
/// is fontdue's `ymin` — the bitmap's *bottom* edge offset from the
/// baseline, negative for descenders (g/j/p/q/y extend below the
/// baseline), positive/zero otherwise. The bitmap's top sits
/// `bearing_y + height` px above the baseline, so its position from the
/// cell top is `ascent - (bearing_y + height)`.
///
/// Pure function so the sign relationship is unit-testable without a GPU
/// — a previous version added `bearing_y` here instead of subtracting it,
/// which rendered every descender well above the baseline instead of
/// straddling it (confirmed bug: see docs/phase5-ui-audit.md).
fn glyph_top_y(ascent: f32, bearing_y: i32, height: u32) -> f32 {
    ascent - bearing_y as f32 - height as f32
}

/// Blends `fg` over `bg` with the given alpha (for selection tinting).
fn blend_over(bg: Rgba, fg: Rgba, alpha: f32) -> Rgba {
    [
        fg[0] * alpha + bg[0] * (1.0 - alpha),
        fg[1] * alpha + bg[1] * (1.0 - alpha),
        fg[2] * alpha + bg[2] * (1.0 - alpha),
        1.0,
    ]
}

/// Attribute bits passed to the glyph shader (must match shader.wgsl).
pub mod attr_bits {
    pub const BOLD: u32 = 1 << 0;
    pub const DIM: u32 = 1 << 1;
    pub const UNDERLINE: u32 = 1 << 2;
    pub const STRIKE: u32 = 1 << 3;
    pub const ITALIC: u32 = 1 << 4;
    pub const BLINK: u32 = 1 << 5;
    pub const REVERSE: u32 = 1 << 6;
    pub const HIDDEN: u32 = 1 << 7;
}

fn attribute_bits(a: Attribute) -> u32 {
    let mut bits = 0u32;
    if a.bold {
        bits |= attr_bits::BOLD;
    }
    if a.dim {
        bits |= attr_bits::DIM;
    }
    if a.underline {
        bits |= attr_bits::UNDERLINE;
    }
    if a.strikethrough {
        bits |= attr_bits::STRIKE;
    }
    if a.italic {
        bits |= attr_bits::ITALIC;
    }
    if a.blink {
        bits |= attr_bits::BLINK;
    }
    if a.reverse {
        bits |= attr_bits::REVERSE;
    }
    if a.hidden {
        bits |= attr_bits::HIDDEN;
    }
    bits
}

// ---------------------------------------------------------------------------
// GPU instance layout
// ---------------------------------------------------------------------------

/// One background quad: solid fill for a cell (also used for selection and
/// the cursor rect via the same pipeline).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct BgInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

/// One glyph quad: textured sample of the atlas at `uv`, tinted with `fg`,
/// decorated per `attrs`.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct GlyphInstance {
    /// Top-left of the *glyph bitmap* in pixels.
    pos: [f32; 2],
    /// Atlas UV rect (x, y, w, h).
    uv: [f32; 4],
    fg: [f32; 4],
    attrs: u32,
    _pad: [u32; 3],
}

/// UV rect for cells with no glyph. The shader scales the quad by
/// `uv_rect.zw * atlas_size`, so w/h must be 0 — a zero-area quad that
/// rasterizes nothing (a w/h of 1 would emit a full-atlas-sized quad).
const EMPTY_UV: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

// ---------------------------------------------------------------------------
// Glyph atlas
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct AtlasEntry {
    /// Position + size in *texture pixels*.
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl AtlasEntry {
    fn uv(&self, tex_size: u32) -> [f32; 4] {
        let s = tex_size as f32;
        [
            self.x as f32 / s,
            self.y as f32 / s,
            self.w as f32 / s,
            self.h as f32 / s,
        ]
    }
}

/// A GPU glyph atlas: an R8Unorm texture, shelf-packed, growing only when
/// full. Packing starts at (1,1); empty glyph instances never sample it
/// (they use a degenerate zero-area quad, see [`EMPTY_UV`]).
struct GlyphAtlas {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: u32,
    entries: HashMap<GlyphRasterConfig, AtlasEntry>,
    /// All uploaded bitmaps (kept for stats; bounded by the atlas).
    bitmaps: Vec<(GlyphRasterConfig, Vec<u8>)>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// Incremented on every grow; the renderer rebuilds its bind group
    /// when this changes (the texture view changed).
    generation: u64,
}

impl GlyphAtlas {
    const PAD: u32 = 1;
    const INITIAL_SIZE: u32 = 1024;

    fn new(device: &wgpu::Device) -> Self {
        let texture = Self::create_texture(device, Self::INITIAL_SIZE);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            size: Self::INITIAL_SIZE,
            entries: HashMap::new(),
            bitmaps: Vec::new(),
            shelf_x: 1,
            shelf_y: 0,
            shelf_h: 0,
            generation: 0,
        }
    }

    fn create_texture(device: &wgpu::Device, size: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Glyph Atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// Packs (or reuses) a glyph. Uploads only new bitmaps. When the atlas
    /// is full it doubles by copying the old contents to the top-left
    /// corner of a new texture, so existing UVs stay valid and only the
    /// packing state resets (see [`GlyphAtlas::grow`]).
    fn ensure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        g: &RasterGlyph,
    ) -> AtlasEntry {
        if let Some(e) = self.entries.get(&g.key) {
            return *e;
        }
        let w = (g.metrics.width).max(1);
        let h = (g.metrics.height).max(1);

        // Fit into the current shelf; start a new shelf if needed.
        if self.shelf_x + w + Self::PAD > self.size {
            self.shelf_y += self.shelf_h + Self::PAD;
            self.shelf_x = 1;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > self.size {
            self.grow(device, queue);
        }

        let x = self.shelf_x;
        let y = self.shelf_y;
        self.shelf_x += w + Self::PAD;
        self.shelf_h = self.shelf_h.max(h);

        let entry = AtlasEntry { x, y, w, h };
        // Zero-width glyphs (e.g. space) rasterize to an empty bitmap; the
        // atlas region must still be defined (wgpu rejects 0-byte uploads
        // and the texture is otherwise uninitialized), so fill the slot
        // with transparent coverage.
        let src: Vec<u8> = if g.bitmap.is_empty() {
            vec![0u8; (w * h) as usize]
        } else {
            Vec::new()
        };
        let source: &[u8] = if src.is_empty() { &g.bitmap } else { &src };
        // Copy the bitmap into the atlas (R8 coverage).
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            source,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.entries.insert(g.key, entry);
        self.bitmaps.push((g.key, g.bitmap.clone()));
        entry
    }

    /// Doubles the texture by copying the old contents to the top-left
    /// corner; all existing UVs remain valid and packing resumes below.
    fn grow(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.generation += 1;
        let old_size = self.size;
        let new_size = old_size * 2;
        let new_texture = Self::create_texture(device, new_size);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Atlas grow"),
        });
        encoder.copy_texture_to_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyTexture {
                texture: &new_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: old_size,
                height: old_size,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        self.texture = new_texture;
        self.view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.size = new_size;
        self.shelf_x = 1;
        self.shelf_y = old_size;
        self.shelf_h = 0;
    }

    /// Rebuilds the bind group (called after a grow changed the view).
    fn rebind(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Renderer bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,

    fonts: FontLibrary,
    cache: GlyphCache,
    atlas: GlyphAtlas,

    bg_buf: wgpu::Buffer,
    glyph_buf: wgpu::Buffer,
    overlay_buf: wgpu::Buffer,
    /// UI chrome (sidebar, tab strip, focus borders) — rects + text drawn
    /// through the SAME atlas/pipelines (Phase 1 §11: shared glyph resources).
    chrome_bg_buf: wgpu::Buffer,
    chrome_glyph_buf: wgpu::Buffer,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    bind_group_layout: wgpu::BindGroupLayout,
    bg_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,

    /// Persistent CPU-side instance arrays (one slot per cell).
    bg_instances: Vec<BgInstance>,
    glyph_instances: Vec<GlyphInstance>,
    /// Overlay rects (beam/underline cursor) drawn after glyphs.
    overlays: Vec<BgInstance>,
    /// Chrome instance lists (sidebar/tab strip/focus borders).
    chrome_bg: Vec<BgInstance>,
    chrome_glyph: Vec<GlyphInstance>,

    cols: u16,
    rows: u16,
    /// Instance-buffer capacity in cells (max single grid or Σ pane cells).
    capacity: usize,
    /// Per-viewport-slot (origin, instance base, cols, rows) from the last
    /// frame, indexed positionally. A pane's dirty tracker only knows about
    /// *content* changes — it has no idea its `pane_base` offset into the
    /// shared instance buffer shifted because a sibling pane resized. When
    /// that happens, only-dirty-rows uploads would leave stale bytes from
    /// whatever used to occupy that buffer range, which is exactly the
    /// ghost-text-after-resize bug this field exists to prevent.
    last_viewport_layout: Vec<(f32, f32, u32, u16, u16)>,

    cursor_style: CursorStyle,
    blink_epoch: Instant,
    /// Atlas grow counter — the bind group must be rebuilt when it bumps.
    atlas_generation: u64,

    // Stats
    glyph_hits: u64,
    glyph_misses: u64,
}

/// Snapshot of renderer counters for benchmarking / telemetry.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderStats {
    pub glyph_cache_hits: u64,
    pub glyph_cache_misses: u64,
    pub atlas_bytes: u64,
    pub atlas_dimension: u32,
    pub cells: usize,
}

/// One pane viewport for [`Renderer::render_multi`]. The renderer only
/// *reads* snapshots — pane state lives in the engine (Phase 1 §3, §12).
#[derive(Debug, Clone, Copy)]
pub struct ViewportRender<'a> {
    pub snapshot: &'a RenderSnapshot<'a>,
    pub dirty: &'a DirtyTracker,
    /// Top-left of the pane viewport in window pixels.
    pub origin: (f32, f32),
}

/// Per-frame constants shared by every pane grid of one frame (bundled so
/// [`Renderer::render_grid_to`] stays under the clippy argument budget).
#[derive(Debug, Clone, Copy)]
struct FrameCtx {
    ascent: f32,
    cursor_style: CursorStyle,
    blink_on: bool,
}

// WGSL uniform layout: vec2 screen_size, vec2 cell_size, f32 atlas_size
// (padded to 32 bytes for 16-byte uniform alignment).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    cell_size: [f32; 2],
    atlas_size: f32,
    _pad: [f32; 3],
}

impl Renderer {
    const ATLAS_INITIAL_SIZE: u32 = 1024;

    /// Creates the GPU pipeline and takes ownership of the font stack.
    ///
    /// The desktop app scans fonts and calls [`GlyphCache::set_font`] with
    /// the primary monospace font *before* constructing the renderer so
    /// cell metrics are known. The window is owned via `Arc` so the surface
    /// can live for `'static`.
    pub async fn new(window: Arc<Window>, fonts: FontLibrary, cache: GlyphCache) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance.create_surface(window).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("FlashTerminal device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .unwrap();

        // Phase 5 UI audit: every color value in this codebase (DEFAULT_BG/
        // DEFAULT_FG here, ANSI 256-color/true-color in `resolve_color`,
        // the desktop app's chrome/state colors) is authored as a naive
        // `channel / 255.0` float — there is no gamma encoding anywhere in
        // this crate or in `shader.wgsl`. An sRGB *surface* format makes
        // the GPU treat every value the shader writes as linear and
        // auto-convert it to sRGB on present, which silently brightens
        // every dark color (a "near-black" #13151C was actually displaying
        // as #595961 — confirmed by decoding the surface's own output).
        // This is why the whole UI looked washed out rather than the
        // deep-contrast theme the color constants were clearly authored
        // to produce. Preferring a non-sRGB format here makes what the
        // shader writes what actually reaches the screen, matching how
        // every color in this codebase has always been written.
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Shared uniform buffer: screen size + cell size.
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Renderer uniforms"),
            contents: bytemuck::bytes_of(&Uniforms {
                screen_size: [size.width as f32, size.height as f32],
                cell_size: [cache.cell_w().max(1.0), cache.cell_h().max(1.0)],
                atlas_size: Self::ATLAS_INITIAL_SIZE as f32,
                _pad: [0.0; 3],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Glyph sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let atlas = GlyphAtlas::new(&device);
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Renderer bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = atlas.rebind(&device, &bind_group_layout, &uniform_buf, &sampler);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Renderer pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let bg_pipeline = Self::make_pipeline(
            &device,
            &pipeline_layout,
            config.format,
            "vs_bg",
            "fs_bg",
            &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<BgInstance>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    0 => Float32x2,
                    1 => Float32x2,
                    2 => Float32x4,
                ],
            }],
        );
        let glyph_pipeline = Self::make_pipeline(
            &device,
            &pipeline_layout,
            config.format,
            "vs_glyph",
            "fs_glyph",
            &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<GlyphInstance>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    0 => Float32x2,
                    1 => Float32x4,
                    2 => Float32x4,
                    3 => Uint32,
                ],
            }],
        );

        // Instance buffers start empty; they are (re)created lazily on the
        // first render with the actual grid size.
        let bg_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Background instances"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Glyph instances"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let overlay_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Cursor overlay instances"),
            size: (std::mem::size_of::<BgInstance>() * 4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let chrome_bg_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Chrome background instances"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let chrome_glyph_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Chrome glyph instances"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            surface,
            device,
            queue,
            config,
            size,
            fonts,
            cache,
            atlas,
            bg_buf,
            glyph_buf,
            overlay_buf,
            chrome_bg_buf,
            chrome_glyph_buf,
            uniform_buf,
            bind_group,
            bind_group_layout,
            bg_pipeline,
            glyph_pipeline,
            sampler,
            bg_instances: Vec::new(),
            glyph_instances: Vec::new(),
            overlays: Vec::with_capacity(4),
            chrome_bg: Vec::new(),
            chrome_glyph: Vec::new(),
            cols: 0,
            rows: 0,
            capacity: 0,
            last_viewport_layout: Vec::new(),
            cursor_style: CursorStyle::Block,
            blink_epoch: Instant::now(),
            atlas_generation: 0,
            glyph_hits: 0,
            glyph_misses: 0,
        }
    }

    fn make_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
        vs: &str,
        fs: &str,
        vertex_layouts: &[wgpu::VertexBufferLayout],
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Terminal shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Terminal render pipeline"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: vs,
                buffers: vertex_layouts,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: fs,
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        })
    }

    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Pixel size of one terminal cell.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cache.cell_w(), self.cache.cell_h())
    }

    /// Terminal grid dimensions that fit in a window of `size`.
    pub fn grid_size_for(&self, size: PhysicalSize<u32>) -> (u16, u16) {
        let (cw, ch) = self.cell_size();
        let cols = ((size.width as f32 / cw).floor() as u16).max(1);
        let rows = ((size.height as f32 / ch).floor() as u16).max(1);
        (cols, rows)
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.cursor_style = style;
    }

    pub fn stats(&self) -> RenderStats {
        let atlas_bytes = self.atlas.bitmaps.iter().map(|(_, b)| b.len() as u64).sum();
        RenderStats {
            glyph_cache_hits: self.glyph_hits,
            glyph_cache_misses: self.glyph_misses,
            atlas_bytes,
            atlas_dimension: self.atlas.size,
            cells: self.cols as usize * self.rows as usize,
        }
    }

    /// Renders one frame from the immutable snapshot and consumed dirty
    /// tracker. `now` drives cursor blinking (500 ms phase).
    /// Renders one frame from the immutable snapshot and consumed dirty
    /// tracker. `now` drives cursor blinking (500 ms phase). Single-pane
    /// path: equivalent to a one-viewport [`Renderer::render_multi`].
    pub fn render(
        &mut self,
        snapshot: &RenderSnapshot,
        dirty: &DirtyTracker,
        now: Instant,
    ) -> Result<(), wgpu::SurfaceError> {
        self.render_viewports(
            &[ViewportRender {
                snapshot,
                dirty,
                origin: (0.0, 0.0),
            }],
            now,
        )
    }

    /// Renders every pane of a workspace in ONE frame: shared glyph atlas,
    /// shared pipelines, a single surface present (§10, §11, §21). The
    /// renderer never owns pane state — it only consumes snapshots.
    pub fn render_multi<'a>(
        &mut self,
        viewports: &[ViewportRender<'a>],
        now: Instant,
    ) -> Result<(), wgpu::SurfaceError> {
        self.render_viewports(viewports, now)
    }

    fn render_viewports<'a>(
        &mut self,
        viewports: &[ViewportRender<'a>],
        now: Instant,
    ) -> Result<(), wgpu::SurfaceError> {
        let (cell_w, cell_h) = self.cell_size();
        let total: usize = viewports
            .iter()
            .map(|v| v.snapshot.cols as usize * v.snapshot.rows as usize)
            .sum();
        self.ensure_capacity(total, cell_w, cell_h);

        // Update uniforms (window + cell size) once per frame.
        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen_size: [self.size.width as f32, self.size.height as f32],
                cell_size: [cell_w, cell_h],
                atlas_size: self.atlas.size as f32,
                _pad: [0.0; 3],
            }),
        );

        self.overlays.clear();
        let frame = FrameCtx {
            ascent: self.cache.ascent(),
            cursor_style: self.cursor_style,
            blink_on: (now.duration_since(self.blink_epoch).as_millis() / 500).is_multiple_of(2),
        };
        self.glyph_hits = 0;
        self.glyph_misses = 0;

        let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(viewports.len());
        let mut new_layout: Vec<(f32, f32, u32, u16, u16)> = Vec::with_capacity(viewports.len());
        let mut base: u32 = 0;
        for (i, v) in viewports.iter().enumerate() {
            let n = v.snapshot.cols as u32 * v.snapshot.rows as u32;
            let layout = (
                v.origin.0,
                v.origin.1,
                base,
                v.snapshot.cols,
                v.snapshot.rows,
            );
            // A layout shift (this pane's slot in the shared instance buffer
            // moved or resized since last frame — e.g. a sibling pane was
            // resized) invalidates every byte at the new offset, not just
            // the rows this pane's own dirty tracker thinks changed.
            let force_full = self.last_viewport_layout.get(i) != Some(&layout);
            self.render_grid_to(v.snapshot, v.dirty, v.origin, base, &frame, force_full);
            ranges.push((base, n));
            new_layout.push(layout);
            base += n;
        }
        self.last_viewport_layout = new_layout;

        // Rebuild the bind group if the atlas grew (texture view changed).
        if self.atlas.generation != self.atlas_generation {
            self.atlas_generation = self.atlas.generation;
            self.bind_group = self.atlas.rebind(
                &self.device,
                &self.bind_group_layout,
                &self.uniform_buf,
                &self.sampler,
            );
        }

        if !self.overlays.is_empty() {
            self.queue
                .write_buffer(&self.overlay_buf, 0, bytemuck::cast_slice(&self.overlays));
        }
        self.draw_panes(&ranges);
        Ok(())
    }

    /// Fills the instance arrays for one pane at `pane_base` cells into the
    /// shared buffers, positions offset by `origin` window pixels, and
    /// uploads only its dirty rows.
    fn render_grid_to(
        &mut self,
        snapshot: &RenderSnapshot,
        dirty: &DirtyTracker,
        origin: (f32, f32),
        pane_base: u32,
        frame: &FrameCtx,
        force_full: bool,
    ) {
        let (cell_w, cell_h) = self.cell_size();
        let (ox, oy) = origin;
        let cols = snapshot.cols;
        let rows = snapshot.rows;
        self.cols = cols;
        self.rows = rows;

        let full = force_full || dirty.full_redraw || dirty.scroll_delta != 0;
        let row_list: Vec<u16> = if full {
            (0..rows).collect()
        } else {
            dirty.dirty_rows()
        };
        let cursor_visible = !snapshot.cursor.is_hidden && frame.blink_on;

        for &r in &row_list {
            let base = pane_base as usize + r as usize * cols as usize;
            let cursor_on_row = cursor_visible && snapshot.cursor.row == r;
            let cursor_col = snapshot.cursor.col as usize;

            for c in 0..cols as usize {
                let cell = snapshot.visible_cell(r, c as u16);
                let selected = snapshot.is_selected(r, c as u16);
                let is_cursor_cell = cursor_on_row && c == cursor_col;
                let (bx, by) = (c as f32 * cell_w + ox, r as f32 * cell_h + oy);

                // ---- Background ----
                let mut bg = resolve_color(cell.color_bg(), DEFAULT_BG, false);
                if selected {
                    bg = blend_over(bg, SELECTION_BG, SELECTION_BG[3]);
                }
                let mut fg = resolve_color(cell.color_fg(), DEFAULT_FG, cell.attribute().bold);
                if cell.attribute().reverse {
                    std::mem::swap(&mut bg, &mut fg);
                }
                if is_cursor_cell && frame.cursor_style == CursorStyle::Block {
                    bg = CURSOR_COLOR;
                    fg = resolve_color(cell.color_bg(), DEFAULT_BG, false);
                }
                self.bg_instances[base + c] = BgInstance {
                    pos: [bx, by],
                    size: [cell_w, cell_h],
                    color: bg,
                };

                // ---- Glyph ----
                let mut inst = GlyphInstance {
                    pos: [bx, by],
                    uv: EMPTY_UV,
                    fg,
                    attrs: 0,
                    _pad: [0; 3],
                };
                if cell.ch != 0 && !cell.is_wide_continuation() {
                    if let Some(ch) = char::from_u32(cell.ch) {
                        if let Some(font) = self.fonts.font_for(ch) {
                            if self.cache.peek(font, ch) {
                                self.glyph_hits += 1;
                            } else {
                                self.glyph_misses += 1;
                            }
                            if let Some(g) = self.cache.glyph(font, ch) {
                                let entry = self.atlas.ensure(&self.device, &self.queue, g);
                                inst.uv = entry.uv(self.atlas.size);
                                inst.pos = [
                                    bx + g.metrics.bearing_x as f32,
                                    by + glyph_top_y(
                                        frame.ascent,
                                        g.metrics.bearing_y,
                                        g.metrics.height,
                                    ),
                                ];
                                inst.attrs = attribute_bits(cell.attribute());
                            }
                        }
                    }
                }
                self.glyph_instances[base + c] = inst;
            }
        }

        // Upload dirty rows (only those re-encoded), at pane offsets.
        let stride = std::mem::size_of::<BgInstance>() as u64;
        let gstride = std::mem::size_of::<GlyphInstance>() as u64;
        for &r in &row_list {
            let off = (pane_base as usize + r as usize * cols as usize) as u64 * stride;
            let bslice = &self.bg_instances[(pane_base as usize + r as usize * cols as usize)
                ..(pane_base as usize + (r as usize + 1) * cols as usize)];
            self.queue
                .write_buffer(&self.bg_buf, off, bytemuck::cast_slice(bslice));
            let goff = (pane_base as usize + r as usize * cols as usize) as u64 * gstride;
            let gslice = &self.glyph_instances[(pane_base as usize + r as usize * cols as usize)
                ..(pane_base as usize + (r as usize + 1) * cols as usize)];
            self.queue
                .write_buffer(&self.glyph_buf, goff, bytemuck::cast_slice(gslice));
        }

        // Cursor overlay (beam / underline) — drawn after glyphs.
        if cursor_visible && frame.cursor_style != CursorStyle::Block {
            let (c, r) = (snapshot.cursor.col as f32, snapshot.cursor.row as f32);
            let (x, y) = (c * cell_w + ox, r * cell_h + oy);
            let rect = match frame.cursor_style {
                CursorStyle::Beam => BgInstance {
                    pos: [x, y],
                    size: [2.0, cell_h],
                    color: CURSOR_COLOR,
                },
                CursorStyle::Underline => BgInstance {
                    pos: [x, y + cell_h - 2.0],
                    size: [cell_w, 2.0],
                    color: UNDERLINE_COLOR,
                },
                CursorStyle::Block => unreachable!(),
            };
            self.overlays.push(rect);
        }
    }

    // ------------------------------------------------------------------
    // UI chrome (sidebar / tab strip / focus borders) — shared atlas
    // ------------------------------------------------------------------

    /// Clears the chrome lists; call once per frame before adding chrome.
    pub fn begin_chrome(&mut self) {
        self.chrome_bg.clear();
        self.chrome_glyph.clear();
    }

    /// Fills a solid rect in chrome space.
    pub fn chrome_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
        self.chrome_bg.push(BgInstance {
            pos: [x, y],
            size: [w, h],
            color,
        });
    }

    /// Draws a `thickness`-px border around a rect (focused-pane outline).
    pub fn chrome_border(&mut self, x: f32, y: f32, w: f32, h: f32, thickness: f32, color: Rgba) {
        self.chrome_rect(x, y, w, thickness, color);
        self.chrome_rect(x, y + h - thickness, w, thickness, color);
        self.chrome_rect(x, y, thickness, h, color);
        self.chrome_rect(x + w - thickness, y, thickness, h, color);
    }

    /// Draws monospace text in chrome space (shared glyph cache/atlas).
    pub fn chrome_text(&mut self, x: f32, y: f32, text: &str, color: Rgba) {
        let (cw, _ch) = self.cell_size();
        let ascent = self.cache.ascent();
        let mut cx = x;
        for ch in text.chars() {
            if ch == '\n' {
                continue;
            }
            if let Some(font) = self.fonts.font_for(ch) {
                if let Some(g) = self.cache.glyph(font, ch) {
                    let entry = self.atlas.ensure(&self.device, &self.queue, g);
                    self.chrome_glyph.push(GlyphInstance {
                        pos: [
                            cx + g.metrics.bearing_x as f32,
                            y + glyph_top_y(ascent, g.metrics.bearing_y, g.metrics.height),
                        ],
                        uv: entry.uv(self.atlas.size),
                        fg: color,
                        attrs: 0,
                        _pad: [0; 3],
                    });
                }
            }
            cx += cw;
        }
    }

    /// Ensures the shared instance buffers hold at least `cells` slots (one
    /// per cell across all panes of the frame). Reallocates only when
    /// growing — steady-state frames reuse the buffers (§21).
    fn ensure_capacity(&mut self, cells: usize, cell_w: f32, cell_h: f32) {
        let (cw, ch) = (cell_w.max(1.0), cell_h.max(1.0));
        if cells <= self.capacity && !self.bg_instances.is_empty() {
            return;
        }
        self.capacity = cells.max(1);
        let n = self.capacity;
        self.bg_instances.clear();
        self.bg_instances.resize(
            n,
            BgInstance {
                pos: [0.0, 0.0],
                size: [cw, ch],
                color: DEFAULT_BG,
            },
        );
        self.glyph_instances.clear();
        self.glyph_instances.resize(
            n,
            GlyphInstance {
                pos: [0.0, 0.0],
                uv: EMPTY_UV,
                fg: DEFAULT_FG,
                attrs: 0,
                _pad: [0; 3],
            },
        );
        let bytes = (n * std::mem::size_of::<BgInstance>()) as u64;
        self.bg_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Background instances"),
            size: bytes.max(4),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let gbytes = (n * std::mem::size_of::<GlyphInstance>()) as u64;
        self.glyph_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Glyph instances"),
            size: gbytes.max(4),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Full initial upload (everything dirty on a fresh grid).
        self.queue
            .write_buffer(&self.bg_buf, 0, bytemuck::cast_slice(&self.bg_instances));
        self.queue.write_buffer(
            &self.glyph_buf,
            0,
            bytemuck::cast_slice(&self.glyph_instances),
        );
    }

    /// Presents one frame: chrome (sidebar/tab strip/borders) first, then
    /// each pane's bg + glyph passes, then cursor overlays — all sharing the
    /// same atlas, pipelines, and a single surface present (§10, §11).
    fn draw_panes(&mut self, ranges: &[(u32, u32)]) {
        // Upload chrome instances if any were added this frame.
        if !self.chrome_bg.is_empty() || !self.chrome_glyph.is_empty() {
            let bbytes = (self.chrome_bg.len() * std::mem::size_of::<BgInstance>()) as u64;
            if self.chrome_bg_buf.size() < bbytes {
                self.chrome_bg_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Chrome background instances"),
                    size: bbytes.max(4),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            let gbytes = (self.chrome_glyph.len() * std::mem::size_of::<GlyphInstance>()) as u64;
            if self.chrome_glyph_buf.size() < gbytes {
                self.chrome_glyph_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Chrome glyph instances"),
                    size: gbytes.max(4),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            if !self.chrome_bg.is_empty() {
                self.queue.write_buffer(
                    &self.chrome_bg_buf,
                    0,
                    bytemuck::cast_slice(&self.chrome_bg),
                );
            }
            if !self.chrome_glyph.is_empty() {
                self.queue.write_buffer(
                    &self.chrome_glyph_buf,
                    0,
                    bytemuck::cast_slice(&self.chrome_glyph),
                );
            }
        }

        let output = match self.surface.get_current_texture() {
            Ok(o) => o,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Err(e) => {
                tracing::warn!("Surface error: {:?}", e);
                return;
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Terminal frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Terminal pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: DEFAULT_BG[0] as f64,
                            g: DEFAULT_BG[1] as f64,
                            b: DEFAULT_BG[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Chrome background (sidebar, tab strip, focus borders).
            if !self.chrome_bg.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.chrome_bg_buf.slice(..));
                pass.draw(0..6, 0..self.chrome_bg.len() as u32);
            }
            // Chrome glyphs (labels).
            if !self.chrome_glyph.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_vertex_buffer(0, self.chrome_glyph_buf.slice(..));
                pass.draw(0..6, 0..self.chrome_glyph.len() as u32);
            }

            // Pane passes (one draw per pane, ranges into the shared buffer).
            for &(base, n) in ranges {
                if n == 0 {
                    continue;
                }
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.bg_buf.slice(..));
                pass.draw(0..6, base..base + n);

                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_vertex_buffer(0, self.glyph_buf.slice(..));
                pass.draw(0..6, base..base + n);
            }

            // Cursor overlays (beam/underline).
            if !self.overlays.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_vertex_buffer(0, self.overlay_buf.slice(..));
                pass.draw(0..6, 0..self.overlays.len() as u32);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the confirmed "descenders render above the
    /// baseline" bug (docs/phase5-ui-audit.md). A non-descender glyph's
    /// bitmap sits entirely above the baseline; a descender's bitmap must
    /// straddle it (bottom edge below, i.e. `top_y + height > ascent`).
    #[test]
    fn glyph_top_y_places_descenders_below_baseline() {
        let ascent = 20.0;
        // Non-descender (e.g. 'x'): bitmap bottom at/above baseline.
        let non_descender_bearing_y = 0;
        let height = 12;
        let top = glyph_top_y(ascent, non_descender_bearing_y, height);
        assert!(
            top + height as f32 <= ascent,
            "a non-descender's bitmap must not extend below the baseline"
        );

        // Descender (e.g. 'p'): fontdue's ymin is negative (bottom edge
        // below baseline). Bitmap bottom must extend below the baseline.
        let descender_bearing_y = -4;
        let top = glyph_top_y(ascent, descender_bearing_y, height);
        assert!(
            top + height as f32 > ascent,
            "a descender's bitmap must extend below the baseline, not float above it"
        );
    }

    /// The buggy formula (`ascent + bearing_y - height`) placed descenders
    /// *higher* than non-descenders — the opposite of correct. Pin the
    /// actual direction: a deeper descender (more negative bearing_y)
    /// must sit lower (larger top_y) on screen, not higher.
    #[test]
    fn deeper_descender_sits_lower_not_higher() {
        let ascent = 20.0;
        let height = 12;
        let shallow = glyph_top_y(ascent, -2, height);
        let deep = glyph_top_y(ascent, -6, height);
        assert!(
            deep > shallow,
            "a deeper descender must render lower on screen (larger y), not higher"
        );
    }

    #[test]
    fn resolve_ansi_16() {
        assert_eq!(
            resolve_color(Color::Indexed(0), DEFAULT_FG, false),
            ANSI_16[0]
        );
        assert_eq!(
            resolve_color(Color::Indexed(7), DEFAULT_FG, false),
            ANSI_16[7]
        );
        // Bold maps the first 8 colors to their bright variants.
        assert_eq!(
            resolve_color(Color::Indexed(1), DEFAULT_FG, true),
            ANSI_16[9]
        );
    }

    #[test]
    fn resolve_256_cube_and_gray() {
        // Cube color 16 = (0,0,0) black.
        assert_eq!(
            resolve_color(Color::Indexed(16), DEFAULT_FG, false),
            [0.0, 0.0, 0.0, 1.0]
        );
        // Gray ramp start.
        assert_eq!(
            resolve_color(Color::Indexed(232), DEFAULT_FG, false),
            [8.0 / 255.0, 8.0 / 255.0, 8.0 / 255.0, 1.0]
        );
    }

    #[test]
    fn resolve_rgb_and_default() {
        assert_eq!(
            resolve_color(Color::Rgb(255, 0, 128), DEFAULT_FG, false),
            [1.0, 0.0, 128.0 / 255.0, 1.0]
        );
        assert_eq!(resolve_color(Color::Default, DEFAULT_FG, false), DEFAULT_FG);
        assert_eq!(resolve_color(Color::Default, DEFAULT_BG, false), DEFAULT_BG);
    }

    #[test]
    fn attribute_bits_mapping() {
        let a = Attribute {
            bold: true,
            underline: true,
            ..Default::default()
        };
        let bits = attribute_bits(a);
        assert_eq!(bits & attr_bits::BOLD, attr_bits::BOLD);
        assert_eq!(bits & attr_bits::UNDERLINE, attr_bits::UNDERLINE);
        assert_eq!(bits & attr_bits::DIM, 0);
    }

    #[test]
    fn atlas_entry_uv_scales() {
        let e = AtlasEntry {
            x: 10,
            y: 20,
            w: 5,
            h: 6,
        };
        let uv = e.uv(100);
        assert_eq!(uv, [0.1, 0.2, 0.05, 0.06]);
    }

    #[test]
    fn blend_over_alpha() {
        let bg = [0.0, 0.0, 0.0, 1.0];
        let fg = [1.0, 0.0, 0.0, 1.0];
        let out = blend_over(bg, fg, 0.5);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!(out[3] == 1.0);
    }
}
