//! Zero-copy DMA-BUF import for Bevy's wgpu/Vulkan renderer.
//!
//! Two pieces:
//!
//! * [`DmabufTexturePlugin`] hooks Bevy's raw Vulkan device creation and enables the extensions the
//!   import needs (`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`,
//!   `VK_EXT_image_drm_format_modifier`, `VK_EXT_queue_family_foreign`). **It must be added before
//!   `DefaultPlugins`** because Bevy reads the settings while building its render plugin.
//! * [`import_dmabuf_texture`] wraps a DMA-BUF as a `wgpu::Texture` with no copy. The memory stays
//!   where the exporter put it; the GPU reads it in place.

use std::{
    ffi::CStr,
    os::fd::{AsRawFd, BorrowedFd, IntoRawFd},
};

use ash::vk;
use bevy::{
    prelude::*,
    render::{
        RenderPlugin,
        renderer::{
            RenderDevice,
            raw_vulkan_init::{AdditionalVulkanFeatures, RawVulkanInitSettings},
        },
    },
};
use tracing::{error, info, warn};
use wgpu::hal::api::Vulkan;

pub use ash;
pub use bevy::render::renderer::raw_vulkan_init;

/// `DRM_FORMAT_MOD_LINEAR`: rows laid out top to bottom with an explicit pitch.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Marker stored in [`AdditionalVulkanFeatures`] when the device was created with every extension
/// the import path requires.
pub struct DmabufImportEnabled;

const REQUIRED_EXTENSIONS: [&CStr; 3] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];
const OPTIONAL_EXTENSIONS: [&CStr; 1] = [ash::ext::queue_family_foreign::NAME];

/// Enables the Vulkan device extensions needed by [`import_dmabuf_texture`].
///
/// Add this plugin **before** `DefaultPlugins`.
pub struct DmabufTexturePlugin;

impl Plugin for DmabufTexturePlugin {
    fn build(&self, app: &mut App) {
        if app.is_plugin_added::<RenderPlugin>() {
            error!(
                "DmabufTexturePlugin was added after RenderPlugin; the Vulkan device is already \
                 configured and DMA-BUF import will be unavailable. Add it before DefaultPlugins."
            );
            return;
        }
        let mut settings = app
            .world_mut()
            .get_resource_or_insert_with(RawVulkanInitSettings::default);
        // SAFETY: the callback only appends extensions the physical device reports as supported
        // and never removes or disables anything wgpu configured.
        unsafe {
            settings.add_create_device_callback(|args, adapter, features| {
                let caps = adapter.physical_device_capabilities();
                let missing: Vec<_> = REQUIRED_EXTENSIONS
                    .iter()
                    .filter(|e| !caps.supports_extension(e))
                    .collect();
                if !missing.is_empty() {
                    warn!("DMA-BUF import unavailable, device lacks {missing:?}");
                    return;
                }
                let api_version = {
                    let instance = adapter.shared_instance().raw_instance();
                    // SAFETY: physical device handle comes from this instance.
                    let props =
                        instance.get_physical_device_properties(adapter.raw_physical_device());
                    props.api_version
                };
                let mut wanted: Vec<&'static CStr> = REQUIRED_EXTENSIONS.to_vec();
                wanted.extend(
                    OPTIONAL_EXTENSIONS
                        .iter()
                        .filter(|e| caps.supports_extension(e)),
                );
                // VK_EXT_image_drm_format_modifier depends on these; they are core from 1.1/1.2.
                if api_version < vk::API_VERSION_1_2 {
                    for dep in [
                        ash::khr::image_format_list::NAME,
                        ash::khr::bind_memory2::NAME,
                        ash::khr::sampler_ycbcr_conversion::NAME,
                        ash::khr::maintenance1::NAME,
                        ash::khr::get_memory_requirements2::NAME,
                    ] {
                        if caps.supports_extension(dep) {
                            wanted.push(dep);
                        }
                    }
                }
                for ext in wanted {
                    if !args.extensions.contains(&ext) {
                        args.extensions.push(ext);
                    }
                }
                features.insert::<DmabufImportEnabled>();
                info!("Vulkan device created with DMA-BUF import extensions");
            });
        }
    }
}

/// Whether the render device can import DMA-BUFs. Callable from the render world.
pub fn import_supported(
    features: Option<&AdditionalVulkanFeatures>,
    device: &RenderDevice,
) -> bool {
    if features.is_some_and(|f| f.has::<DmabufImportEnabled>()) {
        return true;
    }
    // Fall back to inspecting the device, in case the plugin was bypassed.
    // SAFETY: only reads the enabled extension list.
    unsafe { device.wgpu_device().as_hal::<Vulkan>() }.is_some_and(|d| {
        REQUIRED_EXTENSIONS
            .iter()
            .all(|e| d.enabled_device_extensions().contains(e))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum DmabufImportError {
    #[error("render backend is not Vulkan")]
    NotVulkan,
    #[error(
        "Vulkan device was created without {0:?}; add DmabufTexturePlugin before DefaultPlugins"
    )]
    MissingExtension(&'static CStr),
    #[error("texture format {0:?} has no DMA-BUF mapping in this crate")]
    UnsupportedFormat(wgpu::TextureFormat),
    #[error("driver does not list modifier {modifier:#x} for {format:?}")]
    ModifierUnsupported {
        format: wgpu::TextureFormat,
        modifier: u64,
    },
    #[error("driver reports {format:?} with modifier {modifier:#x} is not importable from DMA-BUF")]
    NotImportable {
        format: wgpu::TextureFormat,
        modifier: u64,
    },
    #[error("no memory type satisfies both the image and the DMA-BUF")]
    NoMemoryType,
    #[error("failed to duplicate DMA-BUF fd: {0}")]
    Dup(std::io::Error),
    #[error("{op}: {result:?}")]
    Vulkan {
        op: &'static str,
        result: vk::Result,
    },
}

fn vk_err(op: &'static str) -> impl FnOnce(vk::Result) -> DmabufImportError {
    move |result| DmabufImportError::Vulkan { op, result }
}

/// One plane of a DMA-BUF image. Only single-plane layouts are supported.
pub struct DmabufPlane<'a> {
    pub fd: BorrowedFd<'a>,
    /// Byte offset of the first row inside the buffer.
    pub offset: u64,
    /// Bytes per row.
    pub stride: u32,
}

pub struct DmabufImageDesc<'a> {
    pub label: Option<&'a str>,
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    /// DRM format modifier describing the memory layout. Use [`DRM_FORMAT_MOD_LINEAR`] for
    /// plain row-major buffers such as V4L2 frames.
    pub modifier: u64,
    pub plane: DmabufPlane<'a>,
    /// wgpu usages the texture will be used with. `COPY_SRC` and `TEXTURE_BINDING` are typical.
    pub usage: wgpu::TextureUsages,
}

fn vk_format(format: wgpu::TextureFormat) -> Option<vk::Format> {
    use wgpu::TextureFormat as F;
    Some(match format {
        F::R8Unorm => vk::Format::R8_UNORM,
        F::Rg8Unorm => vk::Format::R8G8_UNORM,
        F::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        F::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
        F::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        F::Bgra8UnormSrgb => vk::Format::B8G8R8A8_SRGB,
        F::R16Unorm => vk::Format::R16_UNORM,
        F::Rg16Unorm => vk::Format::R16G16_UNORM,
        F::Rgba16Unorm => vk::Format::R16G16B16A16_UNORM,
        F::Rgb10a2Unorm => vk::Format::A2B10G10R10_UNORM_PACK32,
        _ => return None,
    })
}

fn vk_usage(usage: wgpu::TextureUsages) -> vk::ImageUsageFlags {
    let mut flags = vk::ImageUsageFlags::empty();
    if usage.contains(wgpu::TextureUsages::COPY_SRC) {
        flags |= vk::ImageUsageFlags::TRANSFER_SRC;
    }
    if usage.contains(wgpu::TextureUsages::COPY_DST) {
        flags |= vk::ImageUsageFlags::TRANSFER_DST;
    }
    if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
        flags |= vk::ImageUsageFlags::SAMPLED;
    }
    if usage.contains(wgpu::TextureUsages::STORAGE_BINDING) {
        flags |= vk::ImageUsageFlags::STORAGE;
    }
    if usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
        flags |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
    }
    flags
}

fn hal_uses(usage: wgpu::TextureUsages) -> wgpu::TextureUses {
    let mut uses = wgpu::TextureUses::empty();
    if usage.contains(wgpu::TextureUsages::COPY_SRC) {
        uses |= wgpu::TextureUses::COPY_SRC;
    }
    if usage.contains(wgpu::TextureUsages::COPY_DST) {
        uses |= wgpu::TextureUses::COPY_DST;
    }
    if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
        uses |= wgpu::TextureUses::RESOURCE;
    }
    if usage.contains(wgpu::TextureUsages::STORAGE_BINDING) {
        uses |= wgpu::TextureUses::STORAGE_READ_WRITE;
    }
    if usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
        uses |= wgpu::TextureUses::COLOR_TARGET;
    }
    uses
}

/// Wrap a DMA-BUF as a `wgpu::Texture` without copying.
///
/// The returned texture owns a duplicate of the file descriptor and the Vulkan image bound to it;
/// dropping the texture releases both. The exporter must keep the underlying buffer alive and must
/// not write to it while the GPU is reading (for V4L2, re-queue the buffer only after the GPU work
/// that read it has completed).
pub fn import_dmabuf_texture(
    device: &wgpu::Device,
    desc: &DmabufImageDesc<'_>,
) -> Result<wgpu::Texture, DmabufImportError> {
    let format = vk_format(desc.format).ok_or(DmabufImportError::UnsupportedFormat(desc.format))?;
    let usage = vk_usage(desc.usage);
    let handle_type = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

    // SAFETY: we only use the hal device for the duration of this function and hand wgpu an image
    // created in accordance with the descriptor we pass to `texture_from_raw`.
    let hal_texture = unsafe {
        let hal_device = device
            .as_hal::<Vulkan>()
            .ok_or(DmabufImportError::NotVulkan)?;
        for ext in REQUIRED_EXTENSIONS {
            if !hal_device.enabled_device_extensions().contains(&ext) {
                return Err(DmabufImportError::MissingExtension(ext));
            }
        }
        let raw: ash::Device = hal_device.raw_device().clone();
        let instance: ash::Instance = hal_device.shared_instance().raw_instance().clone();
        let phd = hal_device.raw_physical_device();

        check_modifier_supported(&instance, phd, desc.format, format, desc.modifier)?;
        check_importable(
            &instance,
            phd,
            desc.format,
            format,
            desc.modifier,
            usage,
            handle_type,
        )?;

        let plane_layouts = [vk::SubresourceLayout {
            offset: desc.plane.offset,
            size: 0,
            row_pitch: desc.plane.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(desc.modifier)
            .plane_layouts(&plane_layouts);
        let mut external_info =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: desc.width,
                height: desc.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info)
            .push_next(&mut modifier_info);
        let image = raw
            .create_image(&image_info, None)
            .map_err(vk_err("vkCreateImage"))?;

        let bind = (|| -> Result<vk::DeviceMemory, DmabufImportError> {
            let mut dedicated_req = vk::MemoryDedicatedRequirements::default();
            let mut req = vk::MemoryRequirements2::default().push_next(&mut dedicated_req);
            raw.get_image_memory_requirements2(
                &vk::ImageMemoryRequirementsInfo2::default().image(image),
                &mut req,
            );
            let size = req.memory_requirements.size;
            let image_type_bits = req.memory_requirements.memory_type_bits;

            let fd_ext = ash::khr::external_memory_fd::Device::new(&instance, &raw);
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            fd_ext
                .get_memory_fd_properties(handle_type, desc.plane.fd.as_raw_fd(), &mut fd_props)
                .map_err(vk_err("vkGetMemoryFdPropertiesKHR"))?;

            let type_bits = image_type_bits & fd_props.memory_type_bits;
            if type_bits == 0 {
                return Err(DmabufImportError::NoMemoryType);
            }
            let memory_type_index = type_bits.trailing_zeros();

            // Vulkan takes ownership of the fd on success, so import a duplicate.
            let dup = desc
                .plane
                .fd
                .try_clone_to_owned()
                .map_err(DmabufImportError::Dup)?;
            let dup_raw = dup.into_raw_fd();
            let mut import_info = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(handle_type)
                .fd(dup_raw);
            let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(size)
                .memory_type_index(memory_type_index)
                .push_next(&mut import_info)
                .push_next(&mut dedicated_info);
            let memory = match raw.allocate_memory(&alloc_info, None) {
                Ok(m) => m,
                Err(e) => {
                    libc_close(dup_raw);
                    return Err(vk_err("vkAllocateMemory (import)")(e));
                }
            };
            if let Err(e) = raw.bind_image_memory(image, memory, 0) {
                raw.free_memory(memory, None);
                return Err(vk_err("vkBindImageMemory")(e));
            }
            Ok(memory)
        })();
        let memory = match bind {
            Ok(m) => m,
            Err(e) => {
                raw.destroy_image(image, None);
                return Err(e);
            }
        };

        let hal_desc = wgpu::hal::TextureDescriptor {
            label: desc.label,
            size: wgpu::Extent3d {
                width: desc.width,
                height: desc.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: desc.format,
            usage: hal_uses(desc.usage),
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let raw_for_drop = raw.clone();
        let drop_callback: wgpu::hal::DropCallback = Box::new(move || {
            // SAFETY: wgpu guarantees no further use of the image once this runs.
            raw_for_drop.destroy_image(image, None);
            raw_for_drop.free_memory(memory, None);
        });
        hal_device.texture_from_raw(
            image,
            &hal_desc,
            Some(drop_callback),
            wgpu::hal::vulkan::TextureMemory::External,
        )
    };

    // SAFETY: `hal_texture` was created respecting this descriptor.
    let texture = unsafe {
        device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: desc.label,
                size: wgpu::Extent3d {
                    width: desc.width,
                    height: desc.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: desc.format,
                usage: desc.usage,
                view_formats: &[],
            },
        )
    };
    Ok(texture)
}

fn libc_close(fd: i32) {
    // SAFETY: we own `fd` and nothing else references it.
    unsafe {
        drop(<std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd));
    }
}

unsafe fn check_modifier_supported(
    instance: &ash::Instance,
    phd: vk::PhysicalDevice,
    wgpu_format: wgpu::TextureFormat,
    format: vk::Format,
    modifier: u64,
) -> Result<(), DmabufImportError> {
    // Two-call pattern: first query the count, then the list.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut props = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: valid handles; ash handles the pNext chain.
    unsafe { instance.get_physical_device_format_properties2(phd, format, &mut props) };
    let count = list.drm_format_modifier_count as usize;
    let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut entries);
    let mut props = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: as above, with an output slice of the reported length.
    unsafe { instance.get_physical_device_format_properties2(phd, format, &mut props) };
    let reported = list.drm_format_modifier_count as usize;
    let found = entries.iter().take(reported).any(|e| {
        e.drm_format_modifier == modifier
            && e.drm_format_modifier_plane_count == 1
            && e.drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::TRANSFER_SRC)
    });
    if found {
        Ok(())
    } else {
        Err(DmabufImportError::ModifierUnsupported {
            format: wgpu_format,
            modifier,
        })
    }
}

unsafe fn check_importable(
    instance: &ash::Instance,
    phd: vk::PhysicalDevice,
    wgpu_format: wgpu::TextureFormat,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    handle_type: vk::ExternalMemoryHandleTypeFlags,
) -> Result<(), DmabufImportError> {
    let mut external_info =
        vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(handle_type);
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .push_next(&mut external_info)
        .push_next(&mut modifier_info);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    // SAFETY: valid handles and well-formed chains.
    unsafe { instance.get_physical_device_image_format_properties2(phd, &info, &mut props) }
        .map_err(|_| DmabufImportError::NotImportable {
            format: wgpu_format,
            modifier,
        })?;
    if external_props
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    {
        Ok(())
    } else {
        Err(DmabufImportError::NotImportable {
            format: wgpu_format,
            modifier,
        })
    }
}

/// Builds a command buffer holding the barrier that makes writes by a non-Vulkan producer (a
/// kernel driver DMA-ing or memcpy-ing into the DMA-BUF) visible to a following transfer or
/// shader read of `texture`.
///
/// wgpu tracks the imported image as plain device memory and inserts no barrier once the image
/// has been used, so without this the GPU may read stale lines from its own caches. Vulkan
/// specifies the fix as a queue-family ownership *acquire* from `VK_QUEUE_FAMILY_FOREIGN_EXT`
/// (plus host-write visibility). Submit the returned buffer immediately before the one that
/// reads the image; wgpu must already consider the image to be in the state matching `layout`
/// (the first use is handled by wgpu's own transition, which precedes every submission).
///
/// It lives in its own command buffer because wgpu does not allow raw Vulkan recording on an
/// encoder that also records wgpu commands. Returns `None` when the backend is not Vulkan.
///
/// `layout` must be the Vulkan layout wgpu keeps the image in for the use that follows:
/// `TRANSFER_SRC_OPTIMAL` for `copy_texture_to_texture`, `SHADER_READ_ONLY_OPTIMAL` for sampling.
///
/// Note: in practice the CPU-side cache write-back in `Capture` is what fixes coherence on
/// x86 with NVIDIA; this barrier is kept because the specification requires it.
pub fn foreign_acquire(
    device: &wgpu::Device,
    texture: &wgpu::Texture,
    layout: vk::ImageLayout,
) -> Option<wgpu::CommandBuffer> {
    ownership_barrier(device, texture, layout, true)
}

/// The matching *release* back to the foreign queue family, to submit after the last read of the
/// frame. Keeps the acquire/release pairs balanced as the spec requires.
pub fn foreign_release(
    device: &wgpu::Device,
    texture: &wgpu::Texture,
    layout: vk::ImageLayout,
) -> Option<wgpu::CommandBuffer> {
    ownership_barrier(device, texture, layout, false)
}

fn ownership_barrier(
    device: &wgpu::Device,
    texture: &wgpu::Texture,
    layout: vk::ImageLayout,
    acquire: bool,
) -> Option<wgpu::CommandBuffer> {
    // SAFETY: we only record barriers on a fresh command buffer, on an image wgpu owns, without
    // changing its layout.
    unsafe {
        let hal_device = device.as_hal::<Vulkan>()?;
        let hal_texture = texture.as_hal::<Vulkan>()?;
        let image = hal_texture.raw_handle();
        let family = hal_device.queue_family_index();
        let raw = hal_device.raw_device().clone();
        let has_foreign = hal_device
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME);
        drop(hal_texture);
        drop(hal_device);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some(if acquire {
                "dmabuf foreign acquire"
            } else {
                "dmabuf foreign release"
            }),
        });
        let recorded = encoder.as_hal_mut::<Vulkan, _, _>(|hal_encoder| {
            let Some(hal_encoder) = hal_encoder else {
                return false;
            };
            let cb = hal_encoder.raw_handle();
            let read_access = vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::SHADER_READ;
            let read_stages = vk::PipelineStageFlags::TRANSFER
                | vk::PipelineStageFlags::FRAGMENT_SHADER
                | vk::PipelineStageFlags::COMPUTE_SHADER;
            let subresource = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };

            if acquire {
                // 1. Host (kernel) writes become visible to device reads. The HOST stage may not
                //    be combined with an ownership transfer, so this is its own barrier.
                let host_visible = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::HOST_WRITE)
                    .dst_access_mask(read_access);
                raw.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::HOST,
                    read_stages,
                    vk::DependencyFlags::empty(),
                    &[host_visible],
                    &[],
                    &[],
                );
                // 2. Ownership acquire from the foreign (non-Vulkan) producer.
                if has_foreign {
                    let take = vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(read_access)
                        .old_layout(layout)
                        .new_layout(layout)
                        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                        .dst_queue_family_index(family)
                        .image(image)
                        .subresource_range(subresource);
                    raw.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        read_stages,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[take],
                    );
                }
            } else if has_foreign {
                let give = vk::ImageMemoryBarrier::default()
                    .src_access_mask(read_access)
                    .dst_access_mask(vk::AccessFlags::empty())
                    .old_layout(layout)
                    .new_layout(layout)
                    .src_queue_family_index(family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .image(image)
                    .subresource_range(subresource);
                raw.cmd_pipeline_barrier(
                    cb,
                    read_stages,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[give],
                );
            }
            true
        });
        recorded.then(|| encoder.finish())
    }
}
