// Vertex shader bindings

struct VertexOutput {
    @location(0) tex_coord: vec2<f32>,
    @builtin(position) position: vec4<f32>,
};

/// Converts a color from sRGB to linear color space.
fn srgb_to_linear(srgb: vec4<f32>) -> vec4<f32> {
    var rgb: vec3<f32> = srgb.rgb;
    if (srgb.a > 0.0) {
        rgb = rgb / srgb.a;
    }
    let a = rgb / 12.92;
    let b = pow((rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    let c = step(vec3<f32>(0.04045), rgb);
    return vec4<f32>(mix(a, b, c) * srgb.a, srgb.a);
}

@vertex
fn vs_main(
    @location(0) a_pos: vec2<f32>,
    @location(1) a_tex_coord: vec2<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    out.tex_coord = a_tex_coord;
    out.position = vec4<f32>(a_pos, 0.0, 1.0);
    return out;
}

// Fragment shader bindings

@group(0) @binding(0) var r_tex_color: texture_2d<f32>;
@group(0) @binding(1) var r_tex_sampler: sampler;

/// Set when the surface is sRGB, so the output has to be converted to linear.
override srgb_framebuffer: bool = false;

fn finish(color: vec4<f32>) -> vec4<f32> {
    if (srgb_framebuffer) {
        return srgb_to_linear(color);
    }
    // We always have a linear texture at the moment.
    return color;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return finish(textureSample(r_tex_color, r_tex_sampler, in.tex_coord));
}

// Post-processing effects.
//
// They run on the finished movie image, which is premultiplied, so color
// channels are kept at or below alpha. `textureSampleLevel` is used because
// it has no uniform control flow requirements.

fn sample_at(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(r_tex_color, r_tex_sampler, uv, 0.0);
}

fn texel_size() -> vec2<f32> {
    return 1.0 / vec2<f32>(textureDimensions(r_tex_color));
}

fn luma(rgb: vec3<f32>) -> f32 {
    return dot(rgb, vec3<f32>(0.299, 0.587, 0.114));
}

const SHARPEN_AMOUNT: f32 = 0.5;

/// Unsharp mask, limited to the range of the direct neighbors so it can't
/// cause halos or ringing.
@fragment
fn fs_sharpen(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = texel_size();
    let c = sample_at(in.tex_coord);
    let n = sample_at(in.tex_coord + vec2<f32>(0.0, -texel.y)).rgb;
    let s = sample_at(in.tex_coord + vec2<f32>(0.0, texel.y)).rgb;
    let w = sample_at(in.tex_coord + vec2<f32>(-texel.x, 0.0)).rgb;
    let e = sample_at(in.tex_coord + vec2<f32>(texel.x, 0.0)).rgb;
    let lo = min(c.rgb, min(min(n, s), min(w, e)));
    let hi = max(c.rgb, max(max(n, s), max(w, e)));
    let sharpened = c.rgb + SHARPEN_AMOUNT * (4.0 * c.rgb - n - s - w - e);
    let rgb = min(clamp(sharpened, lo, hi), vec3<f32>(c.a));
    return finish(vec4<f32>(rgb, c.a));
}

const FXAA_SPAN_MAX: f32 = 8.0;
const FXAA_REDUCE_MUL: f32 = 1.0 / 8.0;
const FXAA_REDUCE_MIN: f32 = 1.0 / 128.0;

/// Fast approximate anti-aliasing (the well-known "FXAA lite" variant).
@fragment
fn fs_fxaa(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = texel_size();
    let uv = in.tex_coord;
    let center = sample_at(uv);
    let luma_nw = luma(sample_at(uv + vec2<f32>(-1.0, -1.0) * texel).rgb);
    let luma_ne = luma(sample_at(uv + vec2<f32>(1.0, -1.0) * texel).rgb);
    let luma_sw = luma(sample_at(uv + vec2<f32>(-1.0, 1.0) * texel).rgb);
    let luma_se = luma(sample_at(uv + vec2<f32>(1.0, 1.0) * texel).rgb);
    let luma_m = luma(center.rgb);
    let luma_min = min(luma_m, min(min(luma_nw, luma_ne), min(luma_sw, luma_se)));
    let luma_max = max(luma_m, max(max(luma_nw, luma_ne), max(luma_sw, luma_se)));

    var dir = vec2<f32>(
        -((luma_nw + luma_ne) - (luma_sw + luma_se)),
        (luma_nw + luma_sw) - (luma_ne + luma_se),
    );
    let dir_reduce = max(
        (luma_nw + luma_ne + luma_sw + luma_se) * (0.25 * FXAA_REDUCE_MUL),
        FXAA_REDUCE_MIN,
    );
    let rcp_dir_min = 1.0 / (min(abs(dir.x), abs(dir.y)) + dir_reduce);
    dir = clamp(dir * rcp_dir_min, vec2<f32>(-FXAA_SPAN_MAX), vec2<f32>(FXAA_SPAN_MAX)) * texel;

    let rgb_a = 0.5 * (
        sample_at(uv + dir * (1.0 / 3.0 - 0.5)).rgb +
        sample_at(uv + dir * (2.0 / 3.0 - 0.5)).rgb
    );
    let rgb_b = rgb_a * 0.5 + 0.25 * (
        sample_at(uv + dir * -0.5).rgb +
        sample_at(uv + dir * 0.5).rgb
    );
    let luma_b = luma(rgb_b);
    let rgb = select(rgb_b, rgb_a, luma_b < luma_min || luma_b > luma_max);
    return finish(vec4<f32>(min(rgb, vec3<f32>(center.a)), center.a));
}

const TAU: f32 = 6.28318530718;

/// Old monitor look: scanlines, an aperture grille and a slight vignette.
@fragment
fn fs_crt(in: VertexOutput) -> @location(0) vec4<f32> {
    let c = sample_at(in.tex_coord);
    // Scanlines with a period of three device pixels.
    let scanline = 0.75 + 0.25 * cos(in.position.y * TAU / 3.0);
    // Every column lets one of red, green and blue through at full strength.
    let column = u32(in.position.x) % 3u;
    let grille = vec3<f32>(
        select(0.8, 1.0, column == 0u),
        select(0.8, 1.0, column == 1u),
        select(0.8, 1.0, column == 2u),
    );
    let d = in.tex_coord - vec2<f32>(0.5);
    let vignette = clamp(1.0 - dot(d, d) * 0.6, 0.0, 1.0);
    // Make up for the light lost to the scanlines and the grille.
    let rgb = c.rgb * scanline * grille * vignette * 1.35;
    return finish(vec4<f32>(min(rgb, vec3<f32>(c.a)), c.a));
}
