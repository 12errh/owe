//! Wallpaper transitions: blending the old image into the new one on the GPU
//! (FR-LIB-3, PRD-F-07).
//!
//! # Where the logic lives, and why
//!
//! Per-transition geometry and masking live in `shaders/transition.wgsl`, because
//! that is where the per-fragment decisions belong; the golden images (see
//! `tests/transitions.rs` and `tests/golden/transitions/`) are the test for them,
//! which is the freeze process the plan prescribes for graphics. What stays in
//! Rust is everything that can be reasoned about with numbers: the transition
//! catalogue, the frame schedule, and the quad maths that decides where each image
//! lands. Those are unit-tested.
//!
//! # Interruption without a snap
//!
//! A new wallpaper requested halfway through a transition must not flash: the
//! blended frame the user is looking at has to become the *source* of the new
//! transition. Doing that with a CPU readback would stall a frame; instead
//! [`TransitionRenderer::retarget`] renders the current blend into an offscreen
//! texture and copies it on the GPU, so the new transition starts from exactly
//! what was on screen. This is the "no snap" rule from BACKEND-DESIGN §5.

use std::time::Duration;

use crate::gpu::{HeadlessGpu, RenderError};
use crate::image::{PixelFormat, QuadPlan, Scaling, plan as quad_plan};

/// Transition frame sizes frozen as goldens: a 720p one, the two common desktop
/// sizes, and a 1440p one — three sizes, so a scale-dependent mask bug cannot hide.
pub const GOLDEN_SIZES: &[(u32, u32)] = &[(1280, 720), (1920, 1080), (2560, 1440)];

/// Edge softness in screen-space uv: ~1.5 px at 1080p. A hard step would alias
/// visibly on a moving wipe, and a wide band would look like a blurry smear.
const EDGE_SOFTNESS_REFERENCE_PIXELS: f32 = 1.5;

/// Wave ripple amplitude, in screen-widths, at mid-transition.
///
/// 0.12 (±7.7 % of the width) is unmistakably a wave at a glance without looking
/// like a glitch.
const WAVE_AMPLITUDE: f32 = 0.12;

/// Wave ripple frequency: cycles per screen height.
///
/// **One** cycle, deliberately. A whole number of cycles is required for the
/// boundary to be continuous across the seam (half a cycle would leave the top and
/// bottom rows meeting at a step), and an *odd* number of half-cycles puts the
/// screen centre on a zero crossing rather than on a peak. With 2.5 cycles the
/// centre sat on a sine peak, so the ripple was symmetric about the middle of the
/// screen and the top and bottom rows bulged identically — visible as a bow-tie
/// rather than a travelling wave. Found by the property test below.
const WAVE_FREQUENCY: f32 = 1.0;

/// The transitions this build can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TransitionKind {
    /// Instant swap: no intermediate frames at all.
    None,
    /// Linear crossfade over the whole screen.
    Fade,
    /// The new image sweeps in from the left edge.
    Wipe,
    /// The new image slides in from the right over a stationary old one.
    Slide,
    /// The new image grows out of the centre (rectangular iris).
    Grow,
    /// A wipe whose boundary is a sine wave.
    Wave,
    /// The new image closes in from the screen edges (circular iris).
    Outer,
}

impl TransitionKind {
    /// Every kind, in the order the config documents them.
    pub const ALL: &'static [TransitionKind] = &[
        TransitionKind::None,
        TransitionKind::Fade,
        TransitionKind::Wipe,
        TransitionKind::Slide,
        TransitionKind::Grow,
        TransitionKind::Wave,
        TransitionKind::Outer,
    ];

    /// Stable id, matching `owe_core::config::KNOWN_TRANSITIONS`.
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionKind::None => "none",
            TransitionKind::Fade => "fade",
            TransitionKind::Wipe => "wipe",
            TransitionKind::Slide => "slide",
            TransitionKind::Grow => "grow",
            TransitionKind::Wave => "wave",
            TransitionKind::Outer => "outer",
        }
    }

    /// Parse a config value. Unknown names are `None` (the config validator
    /// already rejects them with a better message than this could produce).
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|kind| kind.as_str() == name)
    }

    /// The value the shader switches on.
    pub fn code(self) -> u32 {
        match self {
            TransitionKind::None => 0,
            TransitionKind::Fade => 1,
            TransitionKind::Wipe => 2,
            TransitionKind::Slide => 3,
            TransitionKind::Grow => 4,
            TransitionKind::Wave => 5,
            TransitionKind::Outer => 6,
        }
    }

    /// Whether this kind blends at all: `none` needs no intermediate frames and no
    /// second texture, which is what lets a config with `transition = "none"` skip
    /// the whole transition path.
    pub fn is_animated(self) -> bool {
        !matches!(self, TransitionKind::None)
    }
}

/// How many frames a transition renders, and how long it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    frames: u32,
    duration: Duration,
}

impl Schedule {
    /// Build a schedule from a duration in milliseconds and a target frame rate.
    ///
    /// The frame count is `ceil(duration * fps / 1000)` clamped to at least one:
    /// a 100 ms transition at 60 fps renders 6 frames, and a 0 ms transition
    /// renders exactly one (the new image), never zero.
    pub fn new(duration_ms: u64, fps: u32) -> Self {
        let fps = fps.clamp(1, 240);
        if duration_ms == 0 {
            return Self {
                frames: 1,
                duration: Duration::ZERO,
            };
        }
        let exact = (duration_ms as f64 / 1000.0) * f64::from(fps);
        let frames = (exact.ceil() as u32).max(1);
        Self {
            frames,
            duration: Duration::from_millis(duration_ms),
        }
    }

    /// Number of frames this transition renders.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Total duration.
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Progress for a zero-based frame index.
    ///
    /// The last frame is exactly `1.0`, so a transition always ends on the new
    /// image rather than 2 % short of it.
    pub fn progress(&self, frame: u32) -> f32 {
        let denominator = self.frames.max(1) as f32;
        (((frame + 1) as f32) / denominator).min(1.0)
    }

    /// Progress for a frame index, saturating past the end.
    pub fn progress_at_end(&self) -> f32 {
        self.progress(self.frames.saturating_sub(1))
    }

    /// Time between frames (what the daemon sleeps for).
    pub fn frame_interval(&self) -> Duration {
        if self.frames == 0 {
            return Duration::ZERO;
        }
        self.duration / self.frames
    }

    /// Whether `progress` means the transition is over.
    pub fn is_finished(&self, progress: f32) -> bool {
        progress >= 1.0
    }
}

/// The uniform blob handed to the shader: six `vec4`s, 96 bytes, no padding
/// surprises (a struct mixing `f32` and `u32` is where WGSL layout rules bite).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Outgoing image: clip scale, clip offset.
    pub from_quad: [f32; 4],
    /// Outgoing image: uv scale, uv offset.
    pub from_uv: [f32; 4],
    /// Incoming image: clip scale, clip offset.
    pub to_quad: [f32; 4],
    /// Incoming image: uv scale, uv offset.
    pub to_uv: [f32; 4],
    /// Progress, kind code, target width, target height.
    pub control: [f32; 4],
    /// Edge softness, wave amplitude, wave frequency, unused.
    pub extra: [f32; 4],
}

impl Params {
    /// Little-endian bytes for the uniform buffer.
    pub fn to_bytes(self) -> [u8; 96] {
        let mut bytes = [0_u8; 96];
        let groups = [
            self.from_quad,
            self.from_uv,
            self.to_quad,
            self.to_uv,
            self.control,
            self.extra,
        ];
        for (group_index, group) in groups.iter().enumerate() {
            for (slot, value) in group.iter().enumerate() {
                let offset = (group_index * 4 + slot) * 4;
                bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }
}

/// Work out the shader parameters for one frame.
///
/// Pure, and therefore testable: which image lands where is exactly the kind of
/// thing that is invisible in a screenshot until it is wrong on a second monitor.
pub fn params(
    kind: TransitionKind,
    progress: f32,
    target: (u32, u32),
    from_size: (u32, u32),
    to_size: (u32, u32),
    scaling: Scaling,
) -> Params {
    let from: QuadPlan = quad_plan(from_size, target, scaling);
    let to: QuadPlan = quad_plan(to_size, target, scaling);
    let softness = EDGE_SOFTNESS_REFERENCE_PIXELS / (target.0.max(1) as f32);

    Params {
        from_quad: [
            from.clip_scale[0],
            from.clip_scale[1],
            from.clip_offset[0],
            from.clip_offset[1],
        ],
        from_uv: [
            from.uv_scale[0],
            from.uv_scale[1],
            from.uv_offset[0],
            from.uv_offset[1],
        ],
        to_quad: [
            to.clip_scale[0],
            to.clip_scale[1],
            to.clip_offset[0],
            to.clip_offset[1],
        ],
        to_uv: [
            to.uv_scale[0],
            to.uv_scale[1],
            to.uv_offset[0],
            to.uv_offset[1],
        ],
        control: [
            progress.clamp(0.0, 1.0),
            kind.code() as f32,
            target.0 as f32,
            target.1 as f32,
        ],
        extra: [softness.max(1e-5), WAVE_AMPLITUDE, WAVE_FREQUENCY, 0.0],
    }
}

/// One image held on the GPU, with the view the pipelines sample.
struct GpuImage {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
}

impl GpuImage {
    fn new(
        device: &wgpu::Device,
        label: &str,
        size: (u32, u32),
        usage: wgpu::TextureUsages,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Internal images are always RGBA: the decoded bytes are RGBA, and a
            // BGRA internal texture would need a per-pixel swizzle on every
            // upload. Only the final target carries the presenter's format.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            size,
        }
    }
}

/// A running transition: both images uploaded once, one uniform write per frame.
///
/// Keeping the textures alive across frames is what makes this cheap — the
/// alternative (uploading two full-screen images per frame) would push hundreds of
/// megabytes through the bus during a 300 ms fade for no visual benefit.
pub struct TransitionRenderer {
    pipeline_internal: wgpu::RenderPipeline,
    pipeline_out: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    from: GpuImage,
    to: GpuImage,
    /// Offscreen target used to bake the current blend when retargeting.
    blend: GpuImage,
    /// The readback target, in the presenter's pixel format.
    out: GpuImage,
    bind_group: wgpu::BindGroup,
    target: (u32, u32),
    format: PixelFormat,
    scaling: Scaling,
    kind: TransitionKind,
    /// Frames already presented, for the interruption path.
    frames_rendered: u32,
}

impl std::fmt::Debug for TransitionRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransitionRenderer")
            .field("target", &self.target)
            .field("kind", &self.kind)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl TransitionRenderer {
    /// Upload both images and prepare the pipelines.
    ///
    /// `from` and `to` are RGBA8 buffers of the given sizes.
    pub fn new(
        gpu: &HeadlessGpu,
        target: (u32, u32),
        from: (&[u8], (u32, u32)),
        to: (&[u8], (u32, u32)),
        kind: TransitionKind,
        scaling: Scaling,
        format: PixelFormat,
    ) -> Result<Self, RenderError> {
        if target.0 == 0 || target.1 == 0 {
            return Err(RenderError::EmptyTarget {
                width: target.0,
                height: target.1,
            });
        }
        check_buffer(from.1, from.0)?;
        check_buffer(to.1, to.0)?;

        let device = gpu.device();

        let source_usage = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC;
        let mut from_image = GpuImage::new(device, "owe-transition-from", from.1, source_usage);
        let mut to_image = GpuImage::new(device, "owe-transition-to", to.1, source_usage);
        upload_image(gpu, &mut from_image, from.0)?;
        upload_image(gpu, &mut to_image, to.0)?;

        let blend = GpuImage::new(
            device,
            "owe-transition-blend",
            target,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
        );
        let out = GpuImage::new(
            device,
            "owe-transition-out",
            target,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("owe-transition-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("owe-transition-params"),
            size: 96,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("owe-transition-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Linear everywhere: both images are being resized to the panel, and
            // nearest would make the incoming image crawl with aliasing.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("owe-transition-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/transition.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("owe-transition-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline_internal = build_pipeline(
            device,
            &pipeline_layout,
            &shader,
            // The blend texture is always RGBA, so the internal pass can be used
            // with any presenter format.
            wgpu::TextureFormat::Rgba8Unorm,
            "owe-transition-pipeline-internal",
        );
        let pipeline_out = build_pipeline(
            device,
            &pipeline_layout,
            &shader,
            format.to_wgpu(),
            "owe-transition-pipeline-out",
        );

        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &sampler,
            &from_image,
            &to_image,
            &uniforms,
        );

        Ok(Self {
            pipeline_internal,
            pipeline_out,
            bind_group_layout,
            sampler,
            uniforms,
            from: from_image,
            to: to_image,
            blend,
            out,
            bind_group,
            target,
            format,
            scaling,
            kind,
            frames_rendered: 0,
        })
    }

    /// The transition currently being rendered.
    pub fn kind(&self) -> TransitionKind {
        self.kind
    }

    /// Pixel size of the target.
    pub fn target(&self) -> (u32, u32) {
        self.target
    }

    /// Frames rendered so far.
    pub fn frames_rendered(&self) -> u32 {
        self.frames_rendered
    }

    /// Render one frame at `progress` and read it back in the target format.
    pub fn frame(&mut self, gpu: &HeadlessGpu, progress: f32) -> Result<Vec<u8>, RenderError> {
        let params = params(
            self.kind,
            progress,
            self.target,
            self.from.size,
            self.to.size,
            self.scaling,
        );
        gpu.queue()
            .write_buffer(&self.uniforms, 0, &params.to_bytes());

        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("owe-transition-encoder"),
            });
        render_pass(
            &mut encoder,
            &self.pipeline_out,
            &self.bind_group,
            &self.out.view,
        );
        gpu.queue().submit(Some(encoder.finish()));

        self.frames_rendered += 1;
        gpu.read_texture(&self.out.texture, self.target)
    }

    /// Render into a caller-provided texture (used by the golden harness, which
    /// owns its own readback path).
    pub fn frame_into(
        &mut self,
        gpu: &HeadlessGpu,
        progress: f32,
        view: &wgpu::TextureView,
    ) -> Result<(), RenderError> {
        let params = params(
            self.kind,
            progress,
            self.target,
            self.from.size,
            self.to.size,
            self.scaling,
        );
        gpu.queue()
            .write_buffer(&self.uniforms, 0, &params.to_bytes());

        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("owe-transition-encoder-into"),
            });
        render_pass(&mut encoder, &self.pipeline_out, &self.bind_group, view);
        gpu.queue().submit(Some(encoder.finish()));
        self.frames_rendered += 1;
        Ok(())
    }

    /// Swap in a new destination image, continuing from the frame currently on
    /// screen (the no-snap property).
    ///
    /// Steps, all on the GPU: render the blend at `at_progress` into the offscreen
    /// blend texture; upload the new destination; copy the blend into a fresh
    /// "from" texture. Afterwards `progress` starts again at 0, but the image being
    /// blended out is exactly what the user was looking at.
    pub fn retarget(
        &mut self,
        gpu: &HeadlessGpu,
        at_progress: f32,
        to: (&[u8], (u32, u32)),
        kind: TransitionKind,
    ) -> Result<(), RenderError> {
        check_buffer(to.1, to.0)?;

        // 1. Bake what is on screen right now.
        let bake = params(
            self.kind,
            at_progress,
            self.target,
            self.from.size,
            self.to.size,
            self.scaling,
        );
        gpu.queue()
            .write_buffer(&self.uniforms, 0, &bake.to_bytes());

        let frozen = GpuImage::new(
            gpu.device(),
            "owe-transition-frozen",
            self.target,
            wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
        );

        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("owe-transition-retarget"),
            });
        render_pass(
            &mut encoder,
            &self.pipeline_internal,
            &self.bind_group,
            &self.blend.view,
        );
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.blend.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &frozen.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.target.0,
                height: self.target.1,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue().submit(Some(encoder.finish()));

        // 2. Upload the new destination over the old one.
        self.from = frozen;
        upload_image(gpu, &mut self.to, to.0)?;
        self.to.size = to.1;

        // 3. Rebuild the bind group: it references the (new) from/to views.
        self.bind_group = make_bind_group(
            gpu.device(),
            &self.bind_group_layout,
            &self.sampler,
            &self.from,
            &self.to,
            &self.uniforms,
        );
        self.kind = kind;
        self.frames_rendered = 0;
        Ok(())
    }
}

/// Verify a pixel buffer matches its declared size before anything touches it.
fn check_buffer(size: (u32, u32), pixels: &[u8]) -> Result<(), RenderError> {
    let expected = size.0 as usize * size.1 as usize * 4;
    if pixels.len() != expected {
        return Err(RenderError::BufferLength {
            width: size.0,
            height: size.1,
            expected,
            actual: pixels.len(),
        });
    }
    if size.0 == 0 || size.1 == 0 {
        return Err(RenderError::EmptyTarget {
            width: size.0,
            height: size.1,
        });
    }
    Ok(())
}

fn upload_image(gpu: &HeadlessGpu, image: &mut GpuImage, pixels: &[u8]) -> Result<(), RenderError> {
    gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &image.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.size.0 * 4),
            rows_per_image: Some(image.size.1),
        },
        wgpu::Extent3d {
            width: image.size.0,
            height: image.size.1,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    })
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    from: &GpuImage,
    to: &GpuImage,
    uniforms: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("owe-transition-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&from.view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&to.view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: uniforms.as_entire_binding(),
            },
        ],
    })
}

fn render_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    view: &wgpu::TextureView,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("owe-transition-pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..3, 0..1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_matches_the_config_ids() {
        let names: Vec<&str> = TransitionKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["none", "fade", "wipe", "slide", "grow", "wave", "outer"]
        );
        for kind in TransitionKind::ALL {
            assert_eq!(TransitionKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(TransitionKind::parse(" FADE "), Some(TransitionKind::Fade));
        assert_eq!(TransitionKind::parse("explode"), None);
    }

    #[test]
    fn kind_codes_are_stable() {
        // The shader switches on these numbers; changing one changes the visuals.
        for (kind, code) in [
            (TransitionKind::None, 0),
            (TransitionKind::Fade, 1),
            (TransitionKind::Wipe, 2),
            (TransitionKind::Slide, 3),
            (TransitionKind::Grow, 4),
            (TransitionKind::Wave, 5),
            (TransitionKind::Outer, 6),
        ] {
            assert_eq!(kind.code(), code);
        }
        assert!(!TransitionKind::None.is_animated());
        assert!(TransitionKind::Fade.is_animated());
    }

    #[test]
    fn schedule_converts_duration_and_fps_into_frames() {
        let schedule = Schedule::new(300, 60);
        assert_eq!(schedule.frames(), 18);
        assert_eq!(schedule.duration(), Duration::from_millis(300));
        // 300 ms over 18 frames (16.67 ms each), not exactly 16: the frame count
        // rounds up, so the interval is the exact division rather than 1/60 s.
        let interval = schedule.frame_interval().as_micros();
        assert!(
            interval.abs_diff(16_667) <= 1,
            "expected ~16.667 ms, got {interval} us"
        );
    }

    #[test]
    fn schedule_always_has_one_frame_and_ends_at_full_progress() {
        for (duration_ms, fps) in [(0, 60), (1, 1), (16, 30), (450, 60), (2000, 24)] {
            let schedule = Schedule::new(duration_ms, fps);
            assert!(schedule.frames() >= 1, "{duration_ms}ms@{fps}");
            let last = schedule.progress(schedule.frames() - 1);
            assert_eq!(last, 1.0, "{duration_ms}ms@{fps} must end on the new image");
            assert!(schedule.progress(0) > 0.0);
            assert!(schedule.progress(0) <= 1.0);
        }
    }

    #[test]
    fn schedule_progress_is_monotonic_and_clamped() {
        let schedule = Schedule::new(450, 60);
        let mut previous = 0.0;
        for frame in 0..schedule.frames() {
            let progress = schedule.progress(frame);
            assert!(progress > previous, "frame {frame} went backwards");
            assert!(progress <= 1.0);
            previous = progress;
        }
        // Past the end it saturates rather than overshooting.
        assert_eq!(schedule.progress(999), 1.0);
        assert!(schedule.is_finished(1.0));
        assert!(!schedule.is_finished(0.999));
    }

    #[test]
    fn schedule_clamps_absurd_frame_rates() {
        assert_eq!(Schedule::new(1000, 0).frames(), 1, "0 fps becomes 1");
        assert_eq!(
            Schedule::new(1000, 9999).frames(),
            240,
            "clamped to 240 fps"
        );
    }

    #[test]
    fn a_zero_duration_transition_is_a_single_full_frame() {
        let schedule = Schedule::new(0, 60);
        assert_eq!(schedule.frames(), 1);
        assert_eq!(schedule.progress(0), 1.0);
        assert_eq!(schedule.frame_interval(), Duration::ZERO);
    }

    #[test]
    fn params_are_ninety_six_bytes_of_little_endian_floats() {
        let params = params(
            TransitionKind::Fade,
            0.5,
            (1920, 1080),
            (3840, 2160),
            (1920, 1080),
            Scaling::Cover,
        );
        let bytes = params.to_bytes();
        assert_eq!(bytes.len(), 96);

        // control = [progress, kind, width, height]
        assert_eq!(&bytes[64..68], &0.5_f32.to_le_bytes());
        assert_eq!(&bytes[68..72], &1.0_f32.to_le_bytes(), "fade is kind 1");
        assert_eq!(&bytes[72..76], &1920.0_f32.to_le_bytes());
        assert_eq!(&bytes[76..80], &1080.0_f32.to_le_bytes());
    }

    #[test]
    fn params_carry_the_scaling_plan_for_both_images() {
        // A 4:1 image onto a 1:1 target in cover mode samples the middle quarter.
        let params = params(
            TransitionKind::Fade,
            0.0,
            (100, 100),
            (400, 100),
            (100, 100),
            Scaling::Cover,
        );
        assert!((params.from_uv[0] - 0.25).abs() < 1e-6, "{params:?}");
        assert!((params.from_uv[2] - 0.375).abs() < 1e-6, "{params:?}");
        // The incoming image matches the target ratio, so it is shown whole.
        assert!((params.to_uv[0] - 1.0).abs() < 1e-6, "{params:?}");
        assert!(params.to_uv[2].abs() < 1e-6, "{params:?}");
    }

    #[test]
    fn params_clamp_progress_and_scale_softness_with_resolution() {
        let wide = params(
            TransitionKind::Wipe,
            5.0,
            (1920, 1080),
            (1920, 1080),
            (1920, 1080),
            Scaling::Cover,
        );
        assert_eq!(wide.control[0], 1.0, "progress is clamped");

        let small = params(
            TransitionKind::Wipe,
            0.5,
            (192, 108),
            (192, 108),
            (192, 108),
            Scaling::Cover,
        );
        let large = params(
            TransitionKind::Wipe,
            0.5,
            (1920, 1080),
            (1920, 1080),
            (1920, 1080),
            Scaling::Cover,
        );
        assert!(
            small.extra[0] > large.extra[0],
            "the edge band is wider in uv on a smaller target, so it stays ~1.5 px"
        );
        assert!((large.extra[0] * 1920.0 - EDGE_SOFTNESS_REFERENCE_PIXELS).abs() < 1e-3);
    }
}
