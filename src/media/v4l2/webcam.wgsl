// Converts packed 4:2:2 YUV (YUYV / UYVY) straight from the capture buffer to linear RGB.
// The texture is Rgba8Unorm with width = frame_width / 2; each texel holds one pixel pair.
#import bevy_pbr::forward_io::VertexOutput

struct WebcamParams {
    width: u32,
    height: u32,
    flags: u32,
    _pad: u32,
}

const FLAG_MIRROR: u32 = 1u;
const FLAG_UYVY: u32 = 2u;
const FLAG_FULL_RANGE: u32 = 4u;
const FLAG_RGBA: u32 = 8u;

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var frame_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var frame_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> params: WebcamParams;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn chroma_of(px: vec4<f32>) -> vec2<f32> {
    if ((params.flags & FLAG_UYVY) != 0u) {
        return vec2<f32>(px.r, px.b);
    }
    return vec2<f32>(px.g, px.a);
}

fn luma_of(px: vec4<f32>, odd: bool) -> f32 {
    if ((params.flags & FLAG_UYVY) != 0u) {
        return select(px.g, px.a, odd);
    }
    return select(px.r, px.b, odd);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv;
    if ((params.flags & FLAG_MIRROR) != 0u) {
        uv.x = 1.0 - uv.x;
    }
    if ((params.flags & FLAG_RGBA) != 0u) {
        // sRGB texture: the sampler returns linear.
        return vec4<f32>(textureSample(frame_tex, frame_sampler, uv).rgb, 1.0);
    }
    let w = f32(params.width);
    let h = f32(params.height);
    let fx = clamp(uv.x * w, 0.0, w - 0.001);
    let fy = clamp(uv.y * h, 0.0, h - 0.001);
    let x = u32(fx);
    let y = i32(fy);
    let pair = i32(x / 2u);

    // Luma: nearest pixel.
    let px = textureLoad(frame_tex, vec2<i32>(pair, y), 0);
    let luma = luma_of(px, (x & 1u) == 1u);

    // Chroma: linear interpolation between the two nearest pairs (chroma is co-sited with the
    // even luma sample, i.e. at the left edge of each pair).
    let max_pair = i32(params.width / 2u) - 1;
    let c = fx * 0.5 - 0.5;
    let c0 = i32(floor(c));
    let t = c - f32(c0);
    let pa = textureLoad(frame_tex, vec2<i32>(clamp(c0, 0, max_pair), y), 0);
    let pb = textureLoad(frame_tex, vec2<i32>(clamp(c0 + 1, 0, max_pair), y), 0);
    let chroma = mix(chroma_of(pa), chroma_of(pb), t);

    var yy: f32;
    var cb: f32;
    var cr: f32;
    if ((params.flags & FLAG_FULL_RANGE) != 0u) {
        yy = luma;
        cb = chroma.x - 0.5;
        cr = chroma.y - 0.5;
    } else {
        // BT.601 limited range: Y in [16,235], C in [16,240].
        yy = (luma * 255.0 - 16.0) / 219.0;
        cb = (chroma.x * 255.0 - 128.0) / 224.0;
        cr = (chroma.y * 255.0 - 128.0) / 224.0;
    }
    let r = yy + 1.402 * cr;
    let g = yy - 0.344136 * cb - 0.714136 * cr;
    let b = yy + 1.772 * cb;
    let srgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(srgb_to_linear(srgb), 1.0);
}
