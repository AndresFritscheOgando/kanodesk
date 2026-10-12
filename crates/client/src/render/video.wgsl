// Draws one I420 frame into a rectangle of the target (letterboxed, or at its
// original size and scrolled), the host's cursor on top of the video when
// asked, and a text overlay. Everything outside the rectangle is black.
// Colour: BT.601 limited range, matching kanodesk_codec::convert.

struct Params {
    // The video's rectangle in target UV (0..1, y down): left, top, width, height.
    video_rect: vec4<f32>,
    // Number of overlay characters.
    text_len: u32,
    // Screen pixels per font pixel.
    glyph_px: f32,
    // Glyph indices, four per vec4.
    text: array<vec4<u32>, 16>,
    // 3x5 glyph bitmaps (bit row*3+col), four per vec4.
    font: array<vec4<u32>, 16>,
    // Cursor image rectangle in video UV: left, top, width, height.
    cursor_rect: vec4<f32>,
    // Used part of the cursor texture (u, v), and 1 when the cursor is shown.
    cursor_uv: vec4<f32>,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: Params;
@group(0) @binding(5) var cursor_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    // 0..1 across the target, y down.
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the whole target.
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn glyph_at(i: u32) -> u32 {
    return params.text[i / 4u][i % 4u];
}

fn font_bits(g: u32) -> u32 {
    return params.font[g / 4u][g % 4u];
}

// Overlay colour with alpha; alpha 0 outside the overlay box.
// The box sits 8 px from the top-left corner with one font pixel of padding.
fn overlay(px: vec2<f32>) -> vec4<f32> {
    let local = (px - vec2<f32>(8.0, 8.0)) / params.glyph_px;
    let box_w = f32(params.text_len) * 4.0 + 1.0;
    if (params.text_len == 0u || local.x < 0.0 || local.y < 0.0 || local.x >= box_w || local.y >= 7.0) {
        return vec4<f32>(0.0);
    }
    let tx = i32(floor(local.x)) - 1;
    let ty = i32(floor(local.y)) - 1;
    if (tx >= 0 && ty >= 0 && ty < 5) {
        let ci = u32(tx) / 4u;
        let col = u32(tx) % 4u;
        if (ci < params.text_len && col < 3u) {
            let bits = font_bits(glyph_at(ci));
            if (((bits >> (u32(ty) * 3u + col)) & 1u) == 1u) {
                return vec4<f32>(1.0, 1.0, 0.25, 1.0);
            }
        }
    }
    return vec4<f32>(0.0, 0.0, 0.0, 0.75);
}

fn yuv_to_rgb(y: f32, u: f32, v: f32) -> vec3<f32> {
    let yy = 1.164384 * (y - 16.0 / 255.0);
    let uu = u - 0.5;
    let vv = v - 0.5;
    let rgb = vec3<f32>(
        yy + 1.596027 * vv,
        yy - 0.391762 * uu - 0.812968 * vv,
        yy + 2.017232 * uu,
    );
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var color = vec3<f32>(0.0);
    let video_uv = (in.uv - params.video_rect.xy) / params.video_rect.zw;
    if (all(video_uv >= vec2<f32>(0.0)) && all(video_uv <= vec2<f32>(1.0))) {
        // Explicit LOD: these samples sit in non-uniform control flow.
        let y = textureSampleLevel(y_tex, samp, video_uv, 0.0).r;
        let u = textureSampleLevel(u_tex, samp, video_uv, 0.0).r;
        let v = textureSampleLevel(v_tex, samp, video_uv, 0.0).r;
        color = yuv_to_rgb(y, u, v);
        if (params.cursor_uv.z > 0.5) {
            let local = (video_uv - params.cursor_rect.xy) / params.cursor_rect.zw;
            if (all(local >= vec2<f32>(0.0)) && all(local < vec2<f32>(1.0))) {
                let c = textureSampleLevel(cursor_tex, samp, local * params.cursor_uv.xy, 0.0);
                color = mix(color, c.rgb, c.a);
            }
        }
    }
    let o = overlay(in.pos.xy);
    return vec4<f32>(mix(color, o.rgb, o.a), 1.0);
}
