// Transition compositor: draws an old wallpaper and a new one, and blends them
// according to one of the six named transitions (docs/BACKEND-DESIGN.md §5).
//
// Shape of the problem: both images are drawn as "quads" (geometry + sampling
// window) exactly like the P1 image pipeline, and the transition decides the
// *weight* of the new image per fragment. That is why slide and grow need no new
// draw calls — they are the same draw with a different transform and a
// containment test.
//
// Control flow is branch-free on purpose (`select` instead of `if`): `textureSample`
// requires uniform control flow, and a uniform-buffer value is uniform in theory
// but not obviously so to a reader (or to the WGSL uniformity analysis under
// future changes). select() sidesteps the question entirely.
//
// Definitions (frozen by the golden images in tests/golden/transitions/):
//
//   none   instant swap; no blending at all
//   fade   linear crossfade over the whole screen
//   wipe   the new image sweeps in from the left edge
//   slide  the new image slides in from the right, over a stationary old one
//   grow   the new image grows out of a point in the centre (rectangular iris)
//   wave   like wipe, but the boundary is a sine wave (one full cycle top to
//          bottom, so the centre row is a zero crossing and the top and bottom
//          rows bulge in opposite directions); the ripple tapers to zero at both
//          ends so the start and end frames are exact
//   outer  the new image closes in from the screen edges (a circular iris that
//          shrinks towards the centre)

struct Params {
    // Geometry scale.xy and offset.zw in clip space for the outgoing image.
    from_quad: vec4<f32>,
    // Sampling scale.xy and offset.zw (which part of the texture is read).
    from_uv: vec4<f32>,
    // The same two for the incoming image, before per-transition transforms.
    to_quad: vec4<f32>,
    to_uv: vec4<f32>,
    // x = progress (0..1), y = transition kind, z = width, w = height.
    control: vec4<f32>,
    // x = edge softness in screen-space uv, y = wave amplitude,
    // z = wave frequency (cycles per screen height), w = unused.
    extra: vec4<f32>,
};

@group(0) @binding(0) var from_texture: texture_2d<f32>;
@group(0) @binding(1) var tex_sampler: sampler;
@group(0) @binding(2) var to_texture: texture_2d<f32>;
@group(0) @binding(3) var<uniform> params: Params;

const KIND_NONE: u32 = 0u;
const KIND_FADE: u32 = 1u;
const KIND_WIPE: u32 = 2u;
const KIND_SLIDE: u32 = 3u;
const KIND_GROW: u32 = 4u;
const KIND_WAVE: u32 = 5u;
const KIND_OUTER: u32 = 6u;

// Guards against dividing by a zero-sized quad (progress 0 in `grow`, or a
// degenerate source). A tiny epsilon makes the containment test fail cleanly
// instead of producing infinities.
const EPSILON: f32 = 1e-4;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // Fullscreen triangle: (-1,-1), (3,-1), (-1,3) — same as the image pipeline.
    let corner = vec2<f32>(
        f32((index << 1u) & 2u),
        f32(index & 2u),
    ) * 2.0 - vec2<f32>(1.0, 1.0);

    var out: VertexOutput;
    out.position = vec4<f32>(corner, 0.0, 1.0);
    out.uv = (corner + vec2<f32>(1.0, 1.0)) * 0.5;
    return out;
}

// Local coordinates (-1..1, +y up) of a clip-space point within a quad.
fn quad_local(clip: vec2<f32>, quad: vec4<f32>) -> vec2<f32> {
    return (clip - quad.zw) / max(quad.xy, vec2<f32>(EPSILON));
}

// Texture coordinate for a point inside a quad, honouring the sampling window.
fn quad_uv(local: vec2<f32>, sampling: vec4<f32>) -> vec2<f32> {
    let base = vec2<f32>((local.x + 1.0) * 0.5, (1.0 - local.y) * 0.5);
    return base * sampling.xy + sampling.zw;
}

// Maps progress onto [-soft, 1 + soft] so that progress 0 and 1 produce exactly
// zero and exactly full coverage — including at the very first and last pixel,
// which a naive `edge = progress` gets half-right and leaves a visible seam.
fn sweep(progress: f32, soft: f32) -> f32 {
    return -soft + progress * (1.0 + 2.0 * soft);
}

// 0 below `edge`, 1 above it, with a soft band of width `soft` around the edge.
fn above(position: f32, edge: f32, soft: f32) -> f32 {
    return smoothstep(edge - soft, edge + soft, position);
}

// Normalised distance from the screen centre, 1.0 at the corners and circular in
// pixel space rather than uv space (an ellipse in uv is a circle on a non-square
// screen; the goldens would freeze the wrong shape).
fn radius(screen: vec2<f32>) -> f32 {
    let aspect = params.control.z / max(params.control.w, 1.0);
    let scaled = vec2<f32>(aspect, 1.0);
    let offset = (screen - vec2<f32>(0.5)) * 2.0 * scaled;
    return length(offset) / length(scaled);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let resolution = max(params.control.zw, vec2<f32>(1.0, 1.0));
    let progress = clamp(params.control.x, 0.0, 1.0);
    let kind = u32(params.control.y + 0.5);
    let soft = max(params.extra.x, 1e-5);

    // Fragment position in screen-space uv (0..1, y grows downwards) and in clip
    // space (y grows upwards). Both are needed: masks are easier in uv, geometry
    // in clip space.
    let screen = in.position.xy / resolution;
    let clip = vec2<f32>(screen.x * 2.0 - 1.0, 1.0 - screen.y * 2.0);

    // --- outgoing image: never moves.
    let from_uv = quad_uv(quad_local(clip, params.from_quad), params.from_uv);

    // --- incoming image: geometry per transition.
    let is_slide = kind == KIND_SLIDE;
    let is_grow = kind == KIND_GROW;
    // WGSL cannot assign to a swizzle, so each transform replaces the whole vec4.
    var quad_to = params.to_quad;
    // Slide: enter from the right edge (a clip-space offset of 2 is one full
    // screen width to the right).
    quad_to = select(
        quad_to,
        vec4<f32>(quad_to.xy, quad_to.z + 2.0 * (1.0 - progress), quad_to.w),
        is_slide,
    );
    // Grow: scale about the quad's own centre, from nothing to full size.
    quad_to = select(
        quad_to,
        vec4<f32>(max(quad_to.xy, vec2<f32>(EPSILON)) * progress, quad_to.zw),
        is_grow,
    );

    let local_to = quad_local(clip, quad_to);
    let inside = all(abs(local_to) <= vec2<f32>(1.0001));
    let to_uv = quad_uv(local_to, params.to_uv);

    // --- weight of the incoming image per transition.
    let wipe_edge = sweep(progress, soft);
    let wipe = 1.0 - above(screen.x, wipe_edge, soft);

    // The ripple tapers to nothing at both ends so the first and last frames are
    // exactly "all old" and "all new" (a constant ripple leaves a 1-pixel seam).
    let taper = 4.0 * progress * (1.0 - progress);
    let wobble = params.extra.y
        * taper
        * sin(screen.y * max(params.extra.z, 0.0) * 6.2831855);
    let wave = 1.0 - above(screen.x, wipe_edge + wobble, soft);

    // Outer: the threshold runs from outside the screen inwards, so the new image
    // appears at the edges first and closes over the centre last.
    let outer = above(radius(screen), sweep(1.0 - progress, soft), soft);

    var weight = progress; // fade
    weight = select(weight, 1.0, kind == KIND_NONE);
    weight = select(weight, wipe, kind == KIND_WIPE);
    weight = select(weight, wave, kind == KIND_WAVE);
    weight = select(weight, outer, kind == KIND_OUTER);
    weight = select(weight, select(0.0, 1.0, inside), is_slide || is_grow);

    // Clamp before sampling: an out-of-quad coordinate belongs to the other
    // image, and reading garbage (or wrapping) would show as noise.
    let from_px = textureSample(from_texture, tex_sampler, clamp(from_uv, vec2<f32>(0.0), vec2<f32>(1.0)));
    let to_px = textureSample(to_texture, tex_sampler, clamp(to_uv, vec2<f32>(0.0), vec2<f32>(1.0)));
    return mix(from_px, to_px, weight);
}

// The uniform blob is written by Rust as six vec4s; this assertion-free struct
// mirror exists so a future refactor of Params cannot silently change the size.
const PARAMS_BYTES: u32 = 96u;
