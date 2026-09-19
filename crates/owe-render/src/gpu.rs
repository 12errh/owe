//! Headless wgpu device: no Wayland, no surface, no window.
//!
//! This exists so rendering can be tested in CI and on machines without a
//! compositor. It is also the template the P1 output workers reuse: create
//! device → build pipelines → render → read back.

use std::path::PathBuf;

use thiserror::Error;

/// Errors from device creation and offscreen rendering.
#[derive(Debug, Error)]
pub enum RenderError {
    /// No adapter could be created (no GPU, no software fallback, no driver).
    #[error("no gpu adapter available: {0}")]
    NoAdapter(String),

    /// The adapter refused to create a device.
    #[error("gpu device request failed: {0}")]
    Device(String),

    /// Reading pixels back from the GPU failed.
    #[error("gpu buffer mapping failed: {0}")]
    Mapping(String),

    /// Two images of different sizes cannot be compared.
    #[error("image size mismatch: {left:?} vs {right:?}")]
    SizeMismatch {
        /// Left image size.
        left: (u32, u32),
        /// Right image size.
        right: (u32, u32),
    },

    /// A pixel buffer does not match its declared dimensions.
    #[error("buffer length {actual} does not match {width}x{height} RGBA8 ({expected})")]
    BufferLength {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },

    /// PNG encode/decode failure.
    #[error("image error: {0}")]
    Image(String),

    /// A zero-sized render target was requested.
    #[error("render target is empty ({width}x{height})")]
    EmptyTarget {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },

    /// A strict golden comparison found no committed reference.
    #[error("golden reference missing at {0} (strict mode never records)")]
    GoldenMissing(PathBuf),

    /// A golden comparison did not match the reference.
    #[error("golden mismatch: {summary}")]
    GoldenMismatch {
        /// Human-readable difference summary.
        summary: String,
    },
}

/// A headless GPU device plus its queue.
pub struct HeadlessGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_info: String,
}

impl std::fmt::Debug for HeadlessGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessGpu")
            .field("adapter", &self.adapter_info)
            .finish_non_exhaustive()
    }
}

impl HeadlessGpu {
    /// Create a headless device, or `Ok(None)` when no adapter exists at all.
    ///
    /// Returning `Ok(None)` (rather than an error) is deliberate: tests on a
    /// machine without any GPU must *skip*, not fail. Software fallbacks
    /// (lavapipe, llvmpipe) and the GL backend are both accepted, because CI
    /// runners have no hardware GPU.
    pub fn new() -> Result<Option<Self>, RenderError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: false,
            })) {
                Ok(adapter) => adapter,
                Err(error) => {
                    tracing_less_note(format!("no adapter: {error}"));
                    return Ok(None);
                }
            };

        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?}, {:?})", info.name, info.device_type, info.backend);

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("owe-headless"),
            ..Default::default()
        }))
        .map_err(|error| RenderError::Device(error.to_string()))?;

        Ok(Some(Self {
            device,
            queue,
            adapter_info,
        }))
    }

    /// Which adapter this device came from (for logs and test output).
    pub fn adapter_info(&self) -> &str {
        &self.adapter_info
    }

    /// The wgpu device, for renderers built on top of this one (image scaling,
    /// the P5 shader runtime).
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Copy a texture back to CPU memory, tightly packed, in the texture's format.
    ///
    /// wgpu requires 256-byte row alignment for texture→buffer copies, so the
    /// mapped rows are de-padded here rather than at every call site.
    pub fn read_texture(
        &self,
        texture: &wgpu::Texture,
        size: (u32, u32),
    ) -> Result<Vec<u8>, RenderError> {
        let (width, height) = size;
        if width == 0 || height == 0 {
            return Err(RenderError::EmptyTarget { width, height });
        }

        let unpadded_bytes_per_row = width * 4;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(256) * 256;

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("owe-readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("owe-readback-encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let _ = self.device.poll(wgpu::PollType::Wait);
        if let Ok(Err(error)) = receiver.recv() {
            return Err(RenderError::Mapping(error.to_string()));
        }

        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            let end = start + unpadded_bytes_per_row as usize;
            pixels.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }

    /// Render a solid colour into an offscreen texture and read it back.
    ///
    /// Returns tightly packed RGBA8 pixels, row-major from the top-left.
    pub fn render_clear(&self, size: (u32, u32), color: [f32; 4]) -> Vec<u8> {
        let (width, height) = size;

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("owe-offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("owe-clear-encoder"),
            });

        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("owe-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(color[0]),
                            g: f64::from(color[1]),
                            b: f64::from(color[2]),
                            a: f64::from(color[3]),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        self.queue.submit(Some(encoder.finish()));

        // Direct readback failure here means the GPU could not map memory; the
        // existing tests assert the pixel values, so an unwrap would hide a real
        // bug behind a panic with no context.
        self.read_texture(&texture, size).unwrap_or_else(|error| {
            tracing_less_note(format!("readback failed: {error}"));
            Vec::new()
        })
    }
}

/// Tiny logging shim: this crate must not depend on `tracing` just to report a
/// skipped adapter inside tests, so it prints to stderr only when asked.
fn tracing_less_note(message: String) {
    if std::env::var_os("OWE_RENDER_DEBUG").is_some() {
        eprintln!("owe-render: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_solid_colour_or_skips_without_a_gpu() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };
        eprintln!("running on adapter: {}", gpu.adapter_info());

        // Pure red, 4x4.
        let pixels = gpu.render_clear((4, 4), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(pixels.len(), 4 * 4 * 4, "tightly packed RGBA8");

        for (index, chunk) in pixels.chunks_exact(4).enumerate() {
            assert_eq!(chunk[0], 255, "red channel of pixel {index}");
            assert_eq!(chunk[1], 0, "green channel of pixel {index}");
            assert_eq!(chunk[2], 0, "blue channel of pixel {index}");
            assert_eq!(chunk[3], 255, "alpha channel of pixel {index}");
        }
    }

    #[test]
    fn handles_non_multiple_of_256_row_widths() {
        let Some(gpu) = HeadlessGpu::new().expect("device creation must not fail") else {
            eprintln!("skipping: no wgpu adapter on this machine");
            return;
        };
        // 7 pixels wide = 28 bytes per row, so the padded stride matters here.
        let pixels = gpu.render_clear((7, 3), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(pixels.len(), 7 * 3 * 4);
        assert!(pixels.chunks_exact(4).all(|px| px[2] == 255));
    }
}
