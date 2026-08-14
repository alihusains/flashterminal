// FlashTerminal renderer shaders.
//
// Two pipelines share one bind group:
//   binding 0: Uniforms (screen_size, cell_size)
//   binding 1: glyph atlas texture (R8Unorm)
//   binding 2: nearest sampler
//
// Both pipelines expand a unit quad per instance using @builtin(vertex_index)
// (6 vertices per quad, triangle list).

struct Uniforms {
    screen_size: vec2<f32>,
    cell_size: vec2<f32>,
    atlas_size: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

/// Corner of the unit quad for vertex index `vi` (triangle list, 6 verts):
/// (0,0) (1,0) (0,1) (1,0) (1,1) (0,1). Computed arithmetically — this naga
/// version rejects non-constant array indexing entirely.
fn quad_corner(vi: u32) -> vec2<f32> {
    let c = vi % 6u;
    let x = select(1.0, 0.0, c == 0u || c == 3u || c == 5u);
    let y = select(1.0, 0.0, c == 0u || c == 1u || c == 3u);
    return vec2<f32>(x, y);
}

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) cell_pos: vec2<f32>,
    @location(3) @interpolate(flat) attrs: u32,
};

fn ndc(p: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        p.x / u.screen_size.x * 2.0 - 1.0,
        1.0 - p.y / u.screen_size.y * 2.0,
        0.0,
        1.0,
    );
}

// ---------------------------------------------------------------------------
// Background pass (solid cells, selection, cursor rect)
// ---------------------------------------------------------------------------

@vertex
fn vs_bg(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    let corner = quad_corner(vi);
    out.position = ndc(pos + corner * size);
    out.uv = vec2<f32>(0.0, 0.0);
    out.color = color;
    out.cell_pos = pos;
    out.attrs = 0u;
    return out;
}

@fragment
fn fs_bg(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}

// ---------------------------------------------------------------------------
// Glyph pass (atlas text with attribute effects)
// ---------------------------------------------------------------------------

@vertex
fn vs_glyph(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) uv_rect: vec4<f32>,
    @location(2) fg: vec4<f32>,
    @location(3) attrs: u32,
) -> VsOut {
    var out: VsOut;
    // Glyph quads are sized by the glyph bitmap; uv_rect.zw is the bitmap
    // size in UV space relative to the square atlas texture.
    let corner = quad_corner(vi);
    let quad_size = uv_rect.zw * u.atlas_size; // uv w/h * atlas px == px
    out.position = ndc(pos + corner * quad_size);
    out.uv = uv_rect.xy + corner * uv_rect.zw;
    out.color = fg;
    out.cell_pos = pos;
    out.attrs = attrs;
    return out;
}

@fragment
fn fs_glyph(in: VsOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(atlas, samp, in.uv).r;
    var col = vec4<f32>(in.color.rgb, in.color.a * alpha);

    // Dim: halve the intensity.
    if (in.attrs & 2u) != 0u {
        col = vec4<f32>(col.rgb * 0.5, col.a);
    }

    // Underline / strikethrough are drawn procedurally relative to the cell.
    let ypx = (1.0 - in.position.y) * u.screen_size.y * 0.5; // from window top
    let cell_y = ypx - floor(ypx / u.cell_size.y) * u.cell_size.y;
    if (in.attrs & 4u) != 0u {
        let under = cell_y > u.cell_size.y - 3.0 && cell_y < u.cell_size.y - 1.0;
        if under {
            col = vec4<f32>(in.color.rgb, in.color.a);
        }
    }
    if (in.attrs & 8u) != 0u {
        let strike = cell_y > u.cell_size.y * 0.45 && cell_y < u.cell_size.y * 0.55;
        if strike {
            col = vec4<f32>(in.color.rgb, in.color.a);
        }
    }

    // Hidden text: transparent.
    if (in.attrs & 128u) != 0u {
        col.a = 0.0;
    }

    return col;
}
