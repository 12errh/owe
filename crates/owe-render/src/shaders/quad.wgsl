// Fullscreen textured quad, used for wallpaper images (P1) and by the shader
// runtime later (P5). One triangle covers the whole target; the two transforms
// below are what let one pipeline do cover, contain and stretch:
//
//   clip_scale / clip_offset  → where the geometry lands on screen
//   uv_scale   / uv_offset    → which part of the texture is sampled
//
// Holding geometry and sampling separate is what makes the P2 transitions
// (wipe, slide, grow) expressible as parameter changes rather than new shaders.

struct Params {
    clip_scale: vec2<f32>,
    clip_offset: vec2<f32>,
    uv_scale: vec2<f32>,
    uv_offset: vec2<f32>,
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(0) @binding(2) var<uniform> params: Params;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // Fullscreen triangle: (-1,-1), (3,-1), (-1,3).
    let corner = vec2<f32>(
        f32((index << 1u) & 2u),
        f32(index & 2u),
    ) * 2.0 - vec2<f32>(1.0, 1.0);

    var out: VertexOutput;
    out.position = vec4<f32>(corner * params.clip_scale + params.clip_offset, 0.0, 1.0);

    // Clip space is bottom-left origin; images are top-left origin.
    let base_uv = (corner + vec2<f32>(1.0, 1.0)) * 0.5;
    out.uv = vec2<f32>(base_uv.x, 1.0 - base_uv.y) * params.uv_scale + params.uv_offset;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.uv);
}
