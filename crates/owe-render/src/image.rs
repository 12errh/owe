//! Scaling a decoded image onto an output-sized surface, on the GPU.
//!
//! The maths is the interesting part and it is pure: [`plan`] turns
//! (source size, target size, mode) into the two transforms the shader consumes.
//! Keeping it separate means the scaling rules are unit-tested on numbers, and
//! the GPU only has to be right about drawing a rectangle.
//!
//! `Cover` is the default because a wallpaper that does not quite fill the screen
//! (or leaves bars) looks broken, while cropping the edges is what every desktop
//! user already expects.

use crate::gpu::{HeadlessGpu, RenderError};

/// Pixel format of the render target.
///
/// [`PixelFormat::Bgra8`] exists because Wayland's `XRGB8888` is BGRA in memory
/// on little-endian: rendering straight into that layout avoids a per-pixel
/// swizzle of a multi-megabyte buffer on every wallpaper change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// RGBA byte order (what `image` decodes to, and what golden files store).
    Rgba8,
    /// BGRA byte order (Wayland `XRGB8888`).
    Bgra8,
}

impl PixelFormat {
    /// The wgpu format a render target in this pixel order needs.
    pub(crate) fn to_wgpu(self) -> wgpu::TextureFormat {
        match self {
            PixelFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
            PixelFormat::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
        }
    }

    /// Bytes per pixel; all supported formats are 4-byte.
    pub fn bytes_per_pixel(self) -> u32 {
        4
    }
}

/// How an image is fitted onto an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scaling {
    /// Fill the output, cropping the overflow. The default.
    #[default]
    Cover,
    /// Show the whole image, leaving background bars.
    Contain,
    /// Distort to fill the output exactly.
    Stretch,
}

impl Scaling {
    /// Stable id used in config and session state.
    pub fn as_str(self) -> &'static str {
        match self {
            Scaling::Cover => "cover",
            Scaling::Contain => "contain",
            Scaling::Stretch => "stretch",
        }
    }

    /// Parse a config value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cover" | "fill" => Some(Scaling::Cover),
            "contain" | "fit" => Some(Scaling::Contain),
            "stretch" => Some(Scaling::Stretch),
            _ => None,
        }
    }
}

/// The two transforms the shader needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadPlan {
    /// Geometry scale in clip space.
    pub clip_scale: [f32; 2],
    /// Geometry offset in clip space.
    pub clip_offset: [f32; 2],
    /// Texture coordinate scale.
    pub uv_scale: [f32; 2],
    /// Texture coordinate offset.
    pub uv_offset: [f32; 2],
}

impl QuadPlan {
    /// The uniform buffer contents, little-endian f32s (no `bytemuck` needed).
    fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        let values = [
            self.clip_scale,
            self.clip_offset,
            self.uv_scale,
            self.uv_offset,
        ];
        for (index, pair) in values.iter().enumerate() {
            for (axis, value) in pair.iter().enumerate() {
                let offset = (index * 2 + axis) * 4;
                bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }
}

/// Work out the transforms for one (source, target, mode) combination.
///
/// Degenerate inputs (a zero-sized source or target) return an identity plan
/// rather than dividing by zero: an invalid size is caught earlier, and producing
/// `NaN` here would poison the GPU pipeline with no useful error.
pub fn plan(source: (u32, u32), target: (u32, u32), mode: Scaling) -> QuadPlan {
    let identity = QuadPlan {
        clip_scale: [1.0, 1.0],
        clip_offset: [0.0, 0.0],
        uv_scale: [1.0, 1.0],
        uv_offset: [0.0, 0.0],
    };

    if source.0 == 0 || source.1 == 0 || target.0 == 0 || target.1 == 0 {
        return identity;
    }

    let source_w = source.0 as f32;
    let source_h = source.1 as f32;
    let target_w = target.0 as f32;
    let target_h = target.1 as f32;

    match mode {
        Scaling::Stretch => identity,
        Scaling::Cover => {
            // Scale so the *smaller* ratio wins, then show only the matching
            // window of the texture, centred.
            let scale = f64::from(target_w / source_w).max(f64::from(target_h / source_h));
            let visible_u = f64::from(target_w) / (f64::from(source_w) * scale);
            let visible_v = f64::from(target_h) / (f64::from(source_h) * scale);
            QuadPlan {
                clip_scale: [1.0, 1.0],
                clip_offset: [0.0, 0.0],
                uv_scale: [visible_u as f32, visible_v as f32],
                uv_offset: [
                    ((1.0 - visible_u) / 2.0) as f32,
                    ((1.0 - visible_v) / 2.0) as f32,
                ],
            }
        }
        Scaling::Contain => {
            // Shrink the geometry to the largest rectangle the image fits in,
            // sampling the whole texture.
            let scale = f64::from(target_w / source_w).min(f64::from(target_h / source_h));
            let drawn_w = (f64::from(source_w) * scale / f64::from(target_w)) as f32;
            let drawn_h = (f64::from(source_h) * scale / f64::from(target_h)) as f32;
            QuadPlan {
                clip_scale: [drawn_w, drawn_h],
                clip_offset: [0.0, 0.0],
                uv_scale: [1.0, 1.0],
                uv_offset: [0.0, 0.0],
            }
        }
    }
}

/// Render `pixels` (RGBA8, `source` sized) into an offscreen texture of
/// `target` size and read it back in `format`.
pub fn render(
    gpu: &HeadlessGpu,
    target: (u32, u32),
    source: (u32, u32),
    pixels: &[u8],
    mode: Scaling,
    format: PixelFormat,
    background: [f32; 4],
) -> Result<Vec<u8>, RenderError> {
    let expected = source.0 as usize * source.1 as usize * 4;
    if pixels.len() != expected {
        return Err(RenderError::BufferLength {
            width: source.0,
            height: source.1,
            expected,
            actual: pixels.len(),
        });
    }
    if target.0 == 0 || target.1 == 0 {
        return Err(RenderError::EmptyTarget {
            width: target.0,
            height: target.1,
        });
    }

    let device = gpu.device();
    let queue = gpu.queue();

    // Source texture.
    let source_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("owe-source-image"),
        size: wgpu::Extent3d {
            width: source.0,
            height: source.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Decoded pixels are RGBA8; the target format is what the presenter needs.
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(source.0 * 4),
            rows_per_image: Some(source.1),
        },
        wgpu::Extent3d {
            width: source.0,
            height: source.1,
            depth_or_array_layers: 1,
        },
    );

    let source_view = source_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("owe-image-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        // Linear filtering: downscaling a 4K wallpaper to a 1366×768 panel is the
        // common case, and nearest looks visibly aliased.
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });

    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("owe-quad-params"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&uniform, 0, &plan(source, target, mode).to_bytes());

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("owe-quad-bind-group-layout"),
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
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("owe-quad-bind-group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uniform.as_entire_binding(),
            },
        ],
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("owe-quad-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad.wgsl").into()),
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("owe-quad-pipeline-layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    let target_format = format.to_wgpu();
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("owe-quad-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    });

    let target_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("owe-scaled-target"),
        size: wgpu::Extent3d {
            width: target.0,
            height: target.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: target_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("owe-quad-encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("owe-quad-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(background[0]),
                        g: f64::from(background[1]),
                        b: f64::from(background[2]),
                        a: f64::from(background[3]),
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit(Some(encoder.finish()));

    gpu.read_texture(&target_texture, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_is_the_identity() {
        assert_eq!(
            plan((100, 50), (200, 400), Scaling::Stretch),
            QuadPlan {
                clip_scale: [1.0, 1.0],
                clip_offset: [0.0, 0.0],
                uv_scale: [1.0, 1.0],
                uv_offset: [0.0, 0.0],
            }
        );
    }

    #[test]
    fn cover_crops_the_longer_axis_and_centres_the_window() {
        // A 4:1 image onto a 1:1 target: half the width is shown, centred.
        let plan = plan((400, 100), (100, 100), Scaling::Cover);
        assert_eq!(
            plan.clip_scale,
            [1.0, 1.0],
            "geometry still covers the screen"
        );
        assert!((plan.uv_scale[0] - 0.25).abs() < 1e-6, "{plan:?}");
        assert!((plan.uv_scale[1] - 1.0).abs() < 1e-6, "{plan:?}");
        assert!((plan.uv_offset[0] - 0.375).abs() < 1e-6, "{plan:?}");
        assert_eq!(plan.uv_offset[1], 0.0);
    }

    #[test]
    fn cover_of_a_matching_aspect_ratio_shows_everything() {
        let plan = plan((1920, 1080), (1280, 720), Scaling::Cover);
        assert!((plan.uv_scale[0] - 1.0).abs() < 1e-6, "{plan:?}");
        assert!((plan.uv_scale[1] - 1.0).abs() < 1e-6, "{plan:?}");
        // Epsilon, not equality: an aspect-matched pair leaves a ~1e-8 rounding
        // residue in the centring offset, which is invisible but not exactly zero.
        assert!(plan.uv_offset[0].abs() < 1e-6, "{plan:?}");
        assert!(plan.uv_offset[1].abs() < 1e-6, "{plan:?}");
    }

    #[test]
    fn contain_letterboxes_instead_of_cropping() {
        // Same 4:1 image onto a 1:1 target: the whole image, bars top and bottom.
        let plan = plan((400, 100), (100, 100), Scaling::Contain);
        assert!((plan.clip_scale[0] - 1.0).abs() < 1e-6, "{plan:?}");
        assert!((plan.clip_scale[1] - 0.25).abs() < 1e-6, "{plan:?}");
        assert_eq!(plan.clip_offset, [0.0, 0.0], "bodies stay centred");
        assert_eq!(plan.uv_scale, [1.0, 1.0], "the whole texture is sampled");
    }

    #[test]
    fn degenerate_sizes_produce_an_identity_instead_of_nan() {
        for (source, target) in [((0, 10), (10, 10)), ((10, 10), (0, 0)), ((0, 0), (0, 0))] {
            let plan = plan(source, target, Scaling::Cover);
            assert_eq!(plan.clip_scale, [1.0, 1.0]);
            assert!(plan.uv_scale.iter().all(|value| value.is_finite()));
            assert!(plan.uv_offset.iter().all(|value| value.is_finite()));
        }
    }

    #[test]
    fn scaling_ids_round_trip() {
        for mode in [Scaling::Cover, Scaling::Contain, Scaling::Stretch] {
            assert_eq!(Scaling::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Scaling::parse(" COVER "), Some(Scaling::Cover));
        assert_eq!(Scaling::parse("fit"), Some(Scaling::Contain));
        assert_eq!(Scaling::parse("nonsense"), None);
    }

    #[test]
    fn uniform_bytes_are_little_endian_floats() {
        let bytes = plan((2, 2), (2, 2), Scaling::Stretch).to_bytes();
        assert_eq!(bytes.len(), 32);
        // First float is clip_scale.x == 1.0.
        assert_eq!(&bytes[0..4], &1.0_f32.to_le_bytes());
        // Third float is clip_offset.x == 0.0.
        assert_eq!(&bytes[8..12], &0.0_f32.to_le_bytes());
    }

    /// Solid RGBA buffer of `width`×`height`.
    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&rgba);
        }
        pixels
    }

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * width + x) * 4) as usize;
        [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ]
    }

    #[test]
    fn render_scales_a_solid_image_or_skips_without_a_gpu() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        let rendered = render(
            &gpu,
            (16, 9),
            (4, 4),
            &solid(4, 4, [10, 200, 30, 255]),
            Scaling::Cover,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("render");

        assert_eq!(rendered.len(), 16 * 9 * 4);
        for y in 0..9 {
            for x in 0..16 {
                assert_eq!(pixel(&rendered, 16, x, y), [10, 200, 30, 255], "at {x},{y}");
            }
        }
    }

    #[test]
    fn cover_crops_to_the_centre_of_a_wide_image() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        // 8x2 image: red | red | green×4 | blue | blue. Covering this into a
        // square target crops to the middle half horizontally, so every output
        // pixel must come from the green band.
        let mut pixels = Vec::with_capacity(8 * 2 * 4);
        let columns = [
            [255, 0, 0],
            [255, 0, 0],
            [0, 255, 0],
            [0, 255, 0],
            [0, 255, 0],
            [0, 255, 0],
            [0, 0, 255],
            [0, 0, 255],
        ];
        for _ in 0..2 {
            for band in columns {
                pixels.extend_from_slice(&[band[0], band[1], band[2], 255]);
            }
        }
        assert_eq!(
            pixels.len(),
            8 * 2 * 4,
            "fixture must match the source size"
        );

        let rendered = render(
            &gpu,
            (4, 4),
            (8, 2),
            &pixels,
            Scaling::Cover,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("render");

        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(
                    pixel(&rendered, 4, x, y),
                    [0, 255, 0, 255],
                    "cover must crop to the centre band, at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn contain_leaves_background_bars() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        // 8x2 image into 4x4 with contain: full width, 25% height → middle band only.
        let rendered = render(
            &gpu,
            (4, 4),
            (8, 2),
            &solid(8, 2, [0, 255, 0, 255]),
            Scaling::Contain,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("render");

        assert_eq!(pixel(&rendered, 4, 0, 0), [0, 0, 0, 255], "top bar");
        assert_eq!(pixel(&rendered, 4, 3, 3), [0, 0, 0, 255], "bottom bar");

        let middle = (1..3)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .filter(|(x, y)| pixel(&rendered, 4, *x, *y) == [0, 255, 0, 255])
            .count();
        assert!(
            middle > 0,
            "the image band must be drawn somewhere in the middle"
        );
    }

    #[test]
    fn bgra_output_swaps_the_red_and_blue_channels() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        // Why this test exists: Wayland's XRGB8888 wants BGRA bytes, and getting
        // it wrong shows up as a red/blue-swapped wallpaper, which is easy to miss
        // and instantly obvious to a user.
        let rendered = render(
            &gpu,
            (2, 2),
            (2, 2),
            &solid(2, 2, [255, 0, 0, 255]),
            Scaling::Stretch,
            PixelFormat::Bgra8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("render");

        assert_eq!(
            pixel(&rendered, 2, 0, 0),
            [0, 0, 255, 255],
            "an all-red image must read as BGRA (blue byte first)"
        );
    }

    #[test]
    fn a_mismatched_source_buffer_is_rejected_before_touching_the_gpu() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        let error = render(
            &gpu,
            (4, 4),
            (4, 4),
            &[0_u8; 8],
            Scaling::Cover,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap_err();
        assert!(matches!(error, RenderError::BufferLength { .. }), "{error}");
    }

    #[test]
    fn a_zero_sized_target_is_rejected() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };

        let error = render(
            &gpu,
            (0, 0),
            (2, 2),
            &solid(2, 2, [1, 2, 3, 255]),
            Scaling::Cover,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap_err();
        assert!(matches!(error, RenderError::EmptyTarget { .. }), "{error}");
    }
}
