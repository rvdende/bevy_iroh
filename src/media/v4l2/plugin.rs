//! [`WebcamPlugin`] and the [`Webcam`] component: a V4L2 camera on a plane.
//!
//! Data path per frame, raw formats:
//! 1. The kernel driver writes into an `MMAP` buffer that was exported as a DMA-BUF at start-up.
//! 2. The capture thread dequeues it, writes back the CPU cache, and queues the buffer index.
//! 3. The render world records one GPU `copy_texture_to_texture` from the imported DMA-BUF image
//!    into the material's texture, then re-queues the buffer once the GPU signals completion.
//!    No CPU ever touches the pixels.
//! 4. The material's fragment shader converts packed YUV to RGB.
//!
//! MJPEG streams are decoded to RGBA on a thread and uploaded. If DMA-BUF import is unavailable
//! the plugin falls back to `queue.write_texture` from the mmap'd buffer and says so in
//! [`WebcamStats::mode`]. Spawn one [`Webcam`] per device.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bevy::{
    asset::{RenderAssetUsages, embedded_asset},
    image::ImageSampler,
    mesh::MeshVertexBufferLayoutRef,
    pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin},
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        extract_component::{ExtractComponent, ExtractComponentPlugin},
        render_asset::RenderAssets,
        render_resource::{
            AsBindGroup, Extent3d, RenderPipelineDescriptor, ShaderType,
            SpecializedMeshPipelineError, TextureDimension, TextureFormat, TextureUsages,
        },
        renderer::{RenderDevice, RenderQueue},
        sync_component::SyncComponent,
        sync_world::SyncToRenderWorld,
        texture::GpuImage,
    },
    shader::ShaderRef,
};

#[cfg(feature = "dmabuf")]
use super::capture::DequeuedFrame;
use super::capture::{Capture, CaptureConfig, FOURCC_MJPG, FOURCC_UYVY, FOURCC_YUYV, FrameLayout};
use super::mjpeg::Decoder;
use super::select::{CameraFormat, FrameFormat, RequestedFormat, choose};

/// Registers the webcam material and the systems that drive [`Webcam`] entities. Add after
/// `DefaultPlugins`. For the zero-copy path, [`super::DmabufTexturePlugin`] must be added
/// *before* `DefaultPlugins`.
#[derive(Default)]
pub struct WebcamPlugin;

impl Plugin for WebcamPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "webcam.wgsl");
        app.add_plugins((
            MaterialPlugin::<WebcamMaterial>::default(),
            ExtractComponentPlugin::<WebcamFeed>::default(),
        ))
        .add_systems(Update, (start_webcams, log_stats));

        app.sub_app_mut(RenderApp).add_systems(
            Render,
            upload_webcam_frames.in_set(RenderSystems::PrepareResources),
        );
    }
}

/// A camera to show on this entity. Spawn it with a `Transform`; the plugin opens the device on
/// the next update, attaches a [`WebcamFeed`], a plane mesh (unless the entity already has a
/// `Mesh3d`) and a [`WebcamMaterial`], and sets [`WebcamStatus`].
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_iroh::media::v4l2::{CameraFormat, FrameFormat, RequestedFormat, Webcam};
/// # fn setup(mut commands: Commands) {
/// commands.spawn((
///     Webcam::new(RequestedFormat::Closest(CameraFormat::new(1280, 720, FrameFormat::Any, 60))),
///     Transform::from_xyz(0.0, 0.45, 0.0),
/// ));
/// # }
/// ```
#[derive(Component, Clone, Debug)]
#[require(Transform, Visibility, SyncToRenderWorld)]
pub struct Webcam {
    /// The V4L2 node, e.g. `/dev/video0`.
    pub device: std::path::PathBuf,
    /// Which of the device's modes to use.
    pub format: RequestedFormat,
    /// Number of kernel buffers. More tolerate GPU stalls; the consumer always takes the newest
    /// frame, so depth does not add latency by itself.
    pub buffers: u32,
    /// Write back the CPU cache for each frame so a non-snooping GPU sees the driver's writes.
    /// Leave on unless you have verified your platform does not need it.
    pub flush_cpu_cache: bool,
    /// Flip horizontally, like a mirror.
    pub mirror: bool,
    /// Skip DMA-BUF import and always upload through the CPU (for comparison).
    pub force_upload: bool,
    /// Height of the plane the plugin spawns, in world units; width follows the aspect ratio.
    /// Ignored when the entity already has a `Mesh3d`.
    pub plane_height: f32,
    /// Read back the first N GPU copies and compare them byte-for-byte with the kernel buffer.
    pub verify_frames: u32,
    /// Skip the foreign-queue acquire/release barriers (diagnostic).
    pub no_barrier: bool,
}

impl Default for Webcam {
    fn default() -> Self {
        Self {
            device: "/dev/video0".into(),
            format: RequestedFormat::default(),
            buffers: 4,
            flush_cpu_cache: true,
            mirror: false,
            force_upload: false,
            plane_height: 0.9,
            verify_frames: 0,
            no_barrier: false,
        }
    }
}

impl Webcam {
    /// `/dev/video0` with this format request.
    pub fn new(format: RequestedFormat) -> Self {
        Self {
            format,
            ..Default::default()
        }
    }

    /// `/dev/video0` at the given size and rate, format chosen automatically (raw zero-copy if
    /// it reaches the rate, else MJPEG).
    pub fn want(width: u32, height: u32, fps: u32) -> Self {
        Self::new(RequestedFormat::Closest(CameraFormat::new(
            width,
            height,
            FrameFormat::Any,
            fps,
        )))
    }

    /// The largest mode that still runs smoothly.
    pub fn auto() -> Self {
        Self::new(RequestedFormat::HighestResolution)
    }

    pub fn device(mut self, device: impl Into<std::path::PathBuf>) -> Self {
        self.device = device.into();
        self
    }

    pub fn mirror(mut self, mirror: bool) -> Self {
        self.mirror = mirror;
        self
    }

    pub fn plane_height(mut self, height: f32) -> Self {
        self.plane_height = height;
        self
    }
}

/// Outcome of opening the camera, inserted next to [`Webcam`].
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum WebcamStatus {
    Running,
    Failed(String),
}

/// How frames reach the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferMode {
    #[default]
    Unknown,
    /// DMA-BUF imported as a Vulkan image; the GPU reads the kernel buffer directly.
    DmabufZeroCopy,
    /// `queue.write_texture` from the mmap'd buffer.
    CpuUpload,
    /// JPEG frames decoded on a CPU thread, then uploaded as RGBA.
    MjpegDecode,
}

impl TransferMode {
    pub fn label(self) -> &'static str {
        match self {
            TransferMode::DmabufZeroCopy => "DMA-BUF zero-copy",
            TransferMode::CpuUpload => "CPU upload",
            TransferMode::MjpegDecode => "MJPEG CPU decode",
            TransferMode::Unknown => "initialising",
        }
    }
}

/// Live statistics, updated by the render world. Read through [`WebcamFeed::stats`].
#[derive(Debug, Clone, Default)]
pub struct WebcamStats {
    pub mode: TransferMode,
    /// Frames delivered to the GPU.
    pub frames: u64,
    /// Delivery rate, updated once a second.
    pub fps: f32,
    /// Time from the driver's timestamp to the GPU copy being issued, for the last frame.
    /// uvcvideo stamps the *start* of a frame, so this includes the frame's own transfer.
    pub last_latency: Duration,
    /// Frames the driver dropped for lack of free buffers.
    pub driver_dropped: u64,
    /// Frames the driver delivered damaged (USB packet loss).
    pub damaged: u64,
    /// Frames dequeued but skipped because a newer one was already available.
    pub skipped: u64,
    /// Duration of the last CPU cache write-back on the capture thread.
    pub flush_time: Duration,
    /// Duration of the last JPEG decode (MJPEG mode only).
    pub decode_time: Duration,
    /// JPEG frames that failed to decode.
    pub decode_failed: u64,
    pub error: Option<String>,
}

impl WebcamStats {
    /// One line summarising the stream, for an overlay.
    pub fn describe(&self, layout: &FrameLayout) -> String {
        let extra = match self.mode {
            TransferMode::MjpegDecode => {
                format!(
                    "decode {:.1} ms  failed {}",
                    self.decode_time.as_secs_f64() * 1e3,
                    self.decode_failed
                )
            }
            _ => format!("cache flush {:.0} us", self.flush_time.as_secs_f64() * 1e6),
        };
        let mut out = format!(
            "{}x{} {} @ {:.1} fps  |  path: {}  |  capture->GPU {:.1} ms  |  driver drops {}  damaged {}  skipped {}  |  {}",
            layout.width,
            layout.height,
            layout.fourcc,
            self.fps,
            self.mode.label(),
            self.last_latency.as_secs_f64() * 1e3,
            self.driver_dropped,
            self.damaged,
            self.skipped,
            extra
        );
        if let Some(e) = &self.error {
            out.push_str(&format!("  |  import error: {e}"));
        }
        out
    }
}

/// The running capture behind a [`Webcam`] entity. Cloned into the render world each frame.
#[derive(Component, Clone)]
pub struct WebcamFeed {
    pub capture: Arc<Capture>,
    /// The texture the material samples. Raw formats: `Rgba8Unorm`, half the frame width, one
    /// texel per pixel pair. MJPEG: `Rgba8UnormSrgb`, full width.
    pub image: Handle<Image>,
    pub layout: FrameLayout,
    pub stats: Arc<Mutex<WebcamStats>>,
    pub decoder: Option<Arc<Decoder>>,
    gpu: Arc<Mutex<WebcamGpu>>,
    force_upload: bool,
    #[cfg_attr(not(feature = "dmabuf"), allow(dead_code))]
    verify_frames: u32,
    #[cfg_attr(not(feature = "dmabuf"), allow(dead_code))]
    no_barrier: bool,
}

impl SyncComponent for WebcamFeed {
    type Target = WebcamFeed;
}

impl ExtractComponent for WebcamFeed {
    type QueryData = &'static WebcamFeed;
    type QueryFilter = ();
    type Out = WebcamFeed;
    fn extract_component(item: &WebcamFeed) -> Option<Self> {
        Some(item.clone())
    }
}

/// Uniform for [`WebcamMaterial`].
#[derive(ShaderType, Debug, Clone, Copy)]
pub struct WebcamParams {
    pub width: u32,
    pub height: u32,
    pub flags: u32,
    pub _pad: u32,
}

pub const FLAG_MIRROR: u32 = 1;
pub const FLAG_UYVY: u32 = 2;
pub const FLAG_FULL_RANGE: u32 = 4;
/// Texture already holds RGBA; sample it directly.
pub const FLAG_RGBA: u32 = 8;

/// Unlit material that converts the camera's native pixel format in the fragment shader.
#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct WebcamMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub frame: Handle<Image>,
    #[uniform(2)]
    pub params: WebcamParams,
}

impl Material for WebcamMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://bevy_iroh/media/v4l2/webcam.wgsl".into()
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // Visible from both sides.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// Opens the device for every newly added [`Webcam`] and attaches the feed.
#[allow(clippy::type_complexity)]
fn start_webcams(
    mut commands: Commands,
    new: Query<
        (Entity, &Webcam, Has<Mesh3d>),
        (Added<Webcam>, Without<WebcamFeed>, Without<WebcamStatus>),
    >,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WebcamMaterial>>,
) {
    for (entity, webcam, has_mesh) in &new {
        match open_feed(webcam, &mut images) {
            Ok((feed, flags)) => {
                let layout = feed.layout;
                let material = materials.add(WebcamMaterial {
                    frame: feed.image.clone(),
                    params: WebcamParams {
                        width: layout.width,
                        height: layout.height,
                        flags,
                        _pad: 0,
                    },
                });
                let mut e = commands.entity(entity);
                if !has_mesh {
                    let height = webcam.plane_height;
                    let width = height * layout.width as f32 / layout.height as f32;
                    e.insert(Mesh3d(meshes.add(Rectangle::new(width, height))));
                }
                e.insert((MeshMaterial3d(material), feed, WebcamStatus::Running));
                info!(
                    "webcam running: {}x{} {}",
                    layout.width, layout.height, layout.fourcc
                );
            }
            Err(msg) => {
                error!("{msg}");
                commands.entity(entity).insert(WebcamStatus::Failed(msg));
            }
        }
    }
}

fn open_feed(webcam: &Webcam, images: &mut Assets<Image>) -> Result<(WebcamFeed, u32), String> {
    let modes = super::capture::list_modes(&webcam.device)
        .map_err(|e| format!("cannot enumerate {}: {e}", webcam.device.display()))?;
    let choice = choose(&modes, webcam.format).ok_or_else(|| {
        format!(
            "{} offers nothing matching {:?}",
            webcam.device.display(),
            webcam.format
        )
    })?;
    info!(
        "webcam: {} -> {}x{} {} @ {:.0} fps ({})",
        webcam.device.display(),
        choice.width,
        choice.height,
        choice.fourcc,
        choice.fps,
        if choice.raw {
            "raw, zero-copy"
        } else {
            "MJPEG, CPU decode"
        }
    );
    let config = CaptureConfig {
        device: webcam.device.clone(),
        width: choice.width,
        height: choice.height,
        fourcc: choice.fourcc,
        fps: Some(choice.fps.round() as u32),
        buffer_count: webcam.buffers,
        flush_cpu_cache: webcam.flush_cpu_cache,
    };
    let capture = Capture::open(&config).map_err(|e| format!("webcam unavailable: {e}"))?;
    let layout = capture.layout();
    let fourcc = layout.fourcc;
    let mjpeg = fourcc == FOURCC_MJPG;
    if !mjpeg && fourcc != FOURCC_YUYV && fourcc != FOURCC_UYVY {
        return Err(format!(
            "unsupported pixel format {fourcc}; only YUYV, UYVY and MJPG are handled"
        ));
    }
    if !mjpeg && layout.width % 2 != 0 {
        return Err(format!(
            "frame width {} is odd; packed 4:2:2 needs an even width",
            layout.width
        ));
    }

    // Raw 4:2:2: one texel per pixel pair (Y0, U, Y1, V). MJPEG: decoded sRGB RGBA, full width.
    let (tex_width, format) = if mjpeg {
        (layout.width, TextureFormat::Rgba8UnormSrgb)
    } else {
        (layout.width / 2, TextureFormat::Rgba8Unorm)
    };
    let size = Extent3d {
        width: tex_width,
        height: layout.height,
        depth_or_array_layers: 1,
    };
    let mut image = Image::new_uninit(
        size,
        TextureDimension::D2,
        format,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::COPY_SRC;
    image.sampler = if mjpeg {
        ImageSampler::linear()
    } else {
        ImageSampler::nearest()
    };
    let image = images.add(image);

    let mut flags = 0;
    if mjpeg {
        flags |= FLAG_RGBA;
    }
    if webcam.mirror {
        flags |= FLAG_MIRROR;
    }
    if fourcc == FOURCC_UYVY {
        flags |= FLAG_UYVY;
    }
    if layout.full_range {
        flags |= FLAG_FULL_RANGE;
    }

    let capture = Arc::new(capture);
    let decoder = mjpeg.then(|| {
        Arc::new(Decoder::start(
            Arc::clone(&capture),
            layout.width,
            layout.height,
        ))
    });
    Ok((
        WebcamFeed {
            capture,
            image,
            layout,
            stats: Arc::new(Mutex::new(WebcamStats::default())),
            decoder,
            gpu: Arc::new(Mutex::new(WebcamGpu::default())),
            force_upload: webcam.force_upload,
            verify_frames: webcam.verify_frames,
            no_barrier: webcam.no_barrier,
        },
        flags,
    ))
}

/// Logs each feed's stats line every few seconds so the active path and rate are visible
/// without a UI.
fn log_stats(time: Res<Time>, feeds: Query<&WebcamFeed>, mut next: Local<f32>) {
    if time.elapsed_secs() < *next {
        return;
    }
    *next = time.elapsed_secs() + 5.0;
    for feed in &feeds {
        let stats = feed.stats.lock().unwrap();
        if stats.frames > 0 {
            info!("webcam: {}", stats.describe(&feed.layout));
        }
    }
}

/// Render-world state: imported textures, one per V4L2 buffer.
#[derive(Default)]
#[cfg_attr(not(feature = "dmabuf"), allow(dead_code))]
struct WebcamGpu {
    mode: TransferMode,
    imported: Vec<wgpu::Texture>,
    fps_window_start: Option<Instant>,
    fps_window_frames: u32,
    verified: u32,
}

impl WebcamGpu {
    fn count_frame(&mut self, stats: &mut WebcamStats) {
        let now = Instant::now();
        self.fps_window_frames += 1;
        stats.frames += 1;
        match self.fps_window_start {
            None => self.fps_window_start = Some(now),
            Some(start) if now - start >= Duration::from_secs(1) => {
                stats.fps = self.fps_window_frames as f32 / (now - start).as_secs_f32();
                self.fps_window_start = Some(now);
                self.fps_window_frames = 0;
            }
            _ => {}
        }
    }
}

fn upload_webcam_frames(
    feeds: Query<&WebcamFeed>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    images: Res<RenderAssets<GpuImage>>,
    #[cfg(feature = "dmabuf")] features: Option<
        Res<bevy::render::renderer::raw_vulkan_init::AdditionalVulkanFeatures>,
    >,
) {
    for feed in &feeds {
        upload_one(
            feed,
            &device,
            &queue,
            &images,
            #[cfg(feature = "dmabuf")]
            features.as_deref(),
        );
    }
}

fn upload_one(
    shared: &WebcamFeed,
    device: &RenderDevice,
    queue: &RenderQueue,
    images: &RenderAssets<GpuImage>,
    #[cfg(feature = "dmabuf")] features: Option<
        &bevy::render::renderer::raw_vulkan_init::AdditionalVulkanFeatures,
    >,
) {
    let Some(gpu_image) = images.get(&shared.image) else {
        return;
    };
    let mut gpu = shared.gpu.lock().unwrap();

    if gpu.mode == TransferMode::Unknown {
        gpu.mode = decide_mode(
            &mut gpu,
            shared,
            device,
            #[cfg(feature = "dmabuf")]
            features,
        );
        shared.stats.lock().unwrap().mode = gpu.mode;
    }
    if let Some(decoder) = shared.decoder.as_ref() {
        let Some(decoded) = decoder.try_recv_latest() else {
            return;
        };
        let layout = shared.layout;
        queue.write_texture(
            gpu_image.texture.as_image_copy(),
            &decoded.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout.width * 4),
                rows_per_image: None,
            },
            Extent3d {
                width: layout.width,
                height: layout.height,
                depth_or_array_layers: 1,
            },
        );
        let latency = super::capture::monotonic_now().saturating_sub(decoded.timestamp);
        decoder.recycle(decoded.rgba);
        let mut stats = shared.stats.lock().unwrap();
        stats.last_latency = latency;
        stats.decode_time = decoded.decode_time;
        stats.decode_failed = decoder.failed();
        stats.driver_dropped = shared.capture.driver_dropped_count();
        stats.damaged = shared.capture.damaged_count();
        stats.skipped = decoder.skipped();
        gpu.count_frame(&mut stats);
        return;
    }

    let capture = &shared.capture;
    let before = capture.dequeued_count();
    let Some(frame) = capture.try_recv_latest() else {
        debug!("webcam: no new frame (dequeued so far {before})");
        return;
    };
    debug!(
        "webcam: frame seq {} buffer {} mode {:?}",
        frame.sequence, frame.index, gpu.mode
    );
    let latency = frame.age();
    let index = frame.index as usize;
    let layout = shared.layout;
    let size = Extent3d {
        width: layout.width / 2,
        height: layout.height,
        depth_or_array_layers: 1,
    };

    match gpu.mode {
        #[cfg(feature = "dmabuf")]
        TransferMode::DmabufZeroCopy => {
            use super::dmabuf::{ash::vk, foreign_acquire, foreign_release};
            let Some(src) = gpu.imported.get(index) else {
                error!("webcam: frame index {index} has no imported texture");
                capture.requeue(frame.index);
                return;
            };
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("webcam dmabuf copy"),
            });
            let layout_vk = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
            // Barriers live in their own command buffers: wgpu forbids raw Vulkan recording on
            // an encoder that also takes wgpu commands.
            let (acquire, release) = if shared.no_barrier {
                (None, None)
            } else {
                (
                    foreign_acquire(device.wgpu_device(), src, layout_vk),
                    foreign_release(device.wgpu_device(), src, layout_vk),
                )
            };
            encoder.copy_texture_to_texture(
                src.as_image_copy(),
                gpu_image.texture.as_image_copy(),
                size,
            );
            if gpu.verified < shared.verify_frames {
                gpu.verified += 1;
                verify_copy(
                    device,
                    queue,
                    (acquire, encoder, release),
                    &gpu_image.texture,
                    size,
                    shared,
                    &frame,
                );
                capture.requeue(frame.index);
            } else {
                queue.submit(
                    [acquire, Some(encoder.finish()), release]
                        .into_iter()
                        .flatten(),
                );
                // Hand the buffer back only once the GPU has finished reading it.
                let capture = Arc::clone(capture);
                queue.on_submitted_work_done(move || capture.requeue(frame.index));
            }
        }
        TransferMode::CpuUpload => {
            let buffer = &capture.buffers()[index];
            let data = &buffer.as_slice()[..(layout.stride * layout.height) as usize];
            queue.write_texture(
                gpu_image.texture.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layout.stride),
                    rows_per_image: None,
                },
                size,
            );
            // write_texture copies into staging memory synchronously, so the buffer is free now.
            capture.requeue(frame.index);
        }
        _ => unreachable!("mode decided above"),
    }

    let mut stats = shared.stats.lock().unwrap();
    stats.last_latency = latency;
    stats.driver_dropped = capture.driver_dropped_count();
    stats.damaged = capture.damaged_count();
    stats.flush_time = capture.last_flush_time();
    let consumed = capture.dequeued_count().saturating_sub(before);
    stats.skipped += consumed.saturating_sub(1);
    gpu.count_frame(&mut stats);
}

fn decide_mode(
    gpu: &mut WebcamGpu,
    shared: &WebcamFeed,
    device: &RenderDevice,
    #[cfg(feature = "dmabuf")] features: Option<
        &bevy::render::renderer::raw_vulkan_init::AdditionalVulkanFeatures,
    >,
) -> TransferMode {
    if shared.decoder.is_some() {
        info!("webcam: MJPEG stream, decoding on CPU thread");
        return TransferMode::MjpegDecode;
    }
    if shared.force_upload {
        info!("webcam: CPU upload forced by configuration");
        return TransferMode::CpuUpload;
    }
    #[cfg(feature = "dmabuf")]
    {
        if !super::dmabuf::import_supported(features, device) {
            warn!("webcam: DMA-BUF import not supported by this device; using CPU upload");
            return TransferMode::CpuUpload;
        }
        match import_all(device, shared) {
            Ok(textures) => {
                gpu.imported = textures;
                info!(
                    "webcam: {} DMA-BUFs imported, zero-copy path active",
                    gpu.imported.len()
                );
                return TransferMode::DmabufZeroCopy;
            }
            Err(e) => {
                warn!("webcam: DMA-BUF import failed ({e}); using CPU upload");
                shared.stats.lock().unwrap().error = Some(e.to_string());
            }
        }
    }
    #[cfg(not(feature = "dmabuf"))]
    {
        let _ = (gpu, device);
        info!("webcam: built without the `dmabuf` feature; using CPU upload");
    }
    TransferMode::CpuUpload
}

#[cfg(feature = "dmabuf")]
fn import_all(
    device: &RenderDevice,
    shared: &WebcamFeed,
) -> Result<Vec<wgpu::Texture>, super::dmabuf::DmabufImportError> {
    use super::dmabuf::{
        DRM_FORMAT_MOD_LINEAR, DmabufImageDesc, DmabufPlane, import_dmabuf_texture,
    };
    use std::os::fd::AsFd;
    let layout = shared.layout;
    shared
        .capture
        .buffers()
        .iter()
        .map(|buffer| {
            import_dmabuf_texture(
                device.wgpu_device(),
                &DmabufImageDesc {
                    label: Some("webcam dmabuf"),
                    width: layout.width / 2,
                    height: layout.height,
                    format: TextureFormat::Rgba8Unorm,
                    modifier: DRM_FORMAT_MOD_LINEAR,
                    plane: DmabufPlane {
                        fd: buffer.dmabuf.as_fd(),
                        offset: buffer.offset,
                        stride: layout.stride,
                    },
                    usage: TextureUsages::COPY_SRC | TextureUsages::TEXTURE_BINDING,
                },
            )
        })
        .collect()
}

/// Diagnostic: read the GPU-side copy back and diff it against the CPU view of the same kernel
/// buffer. Any mismatch means the GPU read stale or torn data.
#[cfg(feature = "dmabuf")]
fn verify_copy(
    device: &RenderDevice,
    queue: &RenderQueue,
    (acquire, mut encoder, release): (
        Option<wgpu::CommandBuffer>,
        wgpu::CommandEncoder,
        Option<wgpu::CommandBuffer>,
    ),
    dst: &wgpu::Texture,
    size: Extent3d,
    shared: &WebcamFeed,
    frame: &DequeuedFrame,
) {
    let layout = shared.layout;
    let bytes_per_row = layout.stride;
    if !bytes_per_row.is_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) {
        warn!("verify: stride {bytes_per_row} not 256-aligned, skipping");
        return;
    }
    let total = (bytes_per_row * layout.height) as u64;
    let readback = device.wgpu_device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("webcam verify readback"),
        size: total,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        dst.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        size,
    );
    queue.submit(
        [acquire, Some(encoder.finish()), release]
            .into_iter()
            .flatten(),
    );
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    if let Err(e) = device
        .wgpu_device()
        .poll(wgpu::PollType::wait_indefinitely())
    {
        warn!("verify: poll failed: {e:?}");
        return;
    }
    let gpu_bytes = slice.get_mapped_range();
    let cpu_bytes = &shared.capture.buffers()[frame.index as usize].as_slice()[..total as usize];
    let mut bad_lines = 0u64;
    let mut bad_rows = std::collections::BTreeSet::new();
    for (i, (a, b)) in gpu_bytes.chunks(64).zip(cpu_bytes.chunks(64)).enumerate() {
        if a != b {
            bad_lines += 1;
            bad_rows.insert(i as u64 * 64 / bytes_per_row as u64);
        }
    }
    drop(gpu_bytes);
    readback.unmap();
    let total_lines = total / 64;
    if bad_lines == 0 {
        info!(
            "verify: frame seq {} buffer {}: GPU copy matches kernel buffer ({total_lines} cache lines)",
            frame.sequence, frame.index
        );
    } else {
        warn!(
            "verify: frame seq {} buffer {}: {bad_lines}/{total_lines} stale 64-byte lines across {} rows (first rows {:?})",
            frame.sequence,
            frame.index,
            bad_rows.len(),
            bad_rows.iter().take(8).collect::<Vec<_>>()
        );
    }
}
