/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! This module contains the convoluted logic that goes into uploading content into
//! the texture cache's textures.
//!
//! We need to support various combinations of code paths depending on the quirks of
//! each hardware/driver configuration:
//! - direct upload,
//! - staged upload via a pixel buffer object,
//! - staged upload via a direct upload to a staging texture where PBO's aren't supported,
//! - copy from the staging to destination textures, either via blits or batched draw calls.
//!
//! Conceptually a lot of this logic should probably be in the device module, but some code
//! here relies on submitting draw calls via the renderer.


use std::mem;
use std::collections::VecDeque;
use std::time::Duration;
use euclid::Transform3D;
use malloc_size_of::MallocSizeOfOps;
use api::units::*;
use api::{ExternalImageHandler, ExternalImageSource, ImageBufferKind, ImageFormat};
use crate::renderer::{
    Renderer, VertexArrayKind, RendererStats, TextureSampler, TEXTURE_CACHE_DBG_CLEAR_COLOR
};
use crate::internal_types::{
    FastHashMap, TextureUpdateSource, Swizzle, TextureCacheUpdate,
    CacheTextureId, RenderTargetInfo,
};
use crate::device::{
    Device, UploadMethod, Texture, DrawTarget, TextureFlags, TextureUploader,
    TextureFilter,
};
use crate::gpu_types::CopyInstance;
use crate::batch::BatchTextures;
use crate::texture_pack::{GuillotineAllocator, FreeRectSlice};
use crate::profiler;
use crate::render_api::MemoryReport;

pub const BATCH_UPLOAD_TEXTURE_SIZE: DeviceIntSize = DeviceIntSize::new(512, 512);
const BATCH_UPLOAD_FORMAT_COUNT: usize = 4;

/// Upload a number of items to texture cache textures.
///
/// This is the main entry point of the texture cache upload code.
/// See also the module documentation for more information.
pub fn upload_to_texture_cache(
    renderer: &mut Renderer,
    update_list: FastHashMap<CacheTextureId, Vec<TextureCacheUpdate>>,
) {
    if update_list.is_empty() {
        return;
    }

    let mut stats = UploadStats {
        num_draw_calls: 0,
        upload_time: 0,
        cpu_buffer_alloc_time: 0,
        texture_alloc_time: 0,
        cpu_copy_time: 0,
        gpu_copy_commands_time: 0,
        bytes_uploaded: 0,
        items_uploaded: 0,
    };

    let upload_total_start = zeitstempel::now();

    let mut batch_upload_textures = Vec::new();

    // A list of copies that must be performed from the temporary textures to the texture cache.
    let mut batch_upload_copies = Vec::new();

    // For each texture format, this stores a list of staging buffers
    // and a texture allocator for packing the buffers.
    let mut batch_upload_buffers = FastHashMap::default();

    let num_updates = update_list.len();

    for (texture_id, updates) in update_list {
        let texture = &renderer.texture_resolver.texture_cache_map[&texture_id].texture;
        for update in updates {
            // A clear needs no source data, and is never batched.
            if matches!(update.source, TextureUpdateSource::DebugClear) {
                let draw_target = DrawTarget::from_texture(
                    texture,
                    false,
                );
                renderer.device.bind_draw_target(draw_target);
                renderer.device.clear_target(
                    Some(TEXTURE_CACHE_DBG_CLEAR_COLOR),
                    None,
                    Some(draw_target.to_framebuffer_rect(update.rect.to_i32()))
                );

                continue;
            }

            let TextureCacheUpdate { rect, stride, offset, format_override, source } = update;

            stats.items_uploaded += 1;

            let use_batch_upload = renderer.device.use_batched_texture_uploads() &&
                texture.flags().contains(TextureFlags::IS_SHARED_TEXTURE_CACHE) &&
                rect.width() <= BATCH_UPLOAD_TEXTURE_SIZE.width &&
                rect.height() <= BATCH_UPLOAD_TEXTURE_SIZE.height &&
                rect.area() < renderer.device.batched_upload_threshold();

            if use_batch_upload {
                // We upload to the staging texture without a format override, so a
                // batched update's override must be a no-op.
                assert!(format_override.map_or(true, |format| format == texture.get_format()));

                if let TextureUpdateSource::Bytes { ref data } = source {
                    if matches!(renderer.device.upload_method(), &UploadMethod::Immediate)
                        && rect.area() > BATCH_UPLOAD_TEXTURE_SIZE.area() / 2 {
                        skip_staging_buffer(
                            &mut renderer.device,
                            &mut renderer.texture_uploader,
                            &mut renderer.staging_texture_pool,
                            rect,
                            &data[offset as usize ..],
                            stride,
                            texture_id,
                            texture.get_format(),
                            &mut batch_upload_textures,
                            &mut batch_upload_copies,
                            &mut stats,
                        );
                        continue;
                    }
                }

                // Reserve a region of a staging texture for this update. The source
                // is not resolved until the staging texture is uploaded, as we can
                // only have one staging buffer mapped at a time.
                reserve_staging_buffer_region(
                    &mut renderer.device,
                    &mut renderer.staging_texture_pool,
                    rect,
                    offset as usize,
                    stride,
                    source,
                    texture_id,
                    texture.get_format(),
                    &mut batch_upload_buffers,
                    &mut batch_upload_textures,
                    &mut batch_upload_copies,
                    &mut stats,
                );

                continue;
            }

            // Not batched, so resolve the source and upload it directly.
            let dummy_data;
            let data = match source {
                TextureUpdateSource::Bytes { ref data } => {
                    &data[offset as usize ..]
                }
                TextureUpdateSource::External { id, channel_index } => {
                    let handler = renderer.external_image_handler
                        .as_mut()
                        .expect("Found external image, but no handler set!");
                    // The filter is only relevant for NativeTexture external images.
                    match handler.lock(id, channel_index, false).source {
                        ExternalImageSource::RawData(data) => {
                            &data[offset as usize ..]
                        }
                        ExternalImageSource::Invalid => {
                            // Create a local buffer to fill the pbo.
                            let bpp = texture.get_format().bytes_per_pixel();
                            let width = stride.unwrap_or(rect.width() * bpp);
                            let total_size = width * rect.height();
                            // WR haven't support RGBAF32 format in texture_cache, so
                            // we use u8 type here.
                            dummy_data = vec![0xFFu8; total_size as usize];
                            &dummy_data
                        }
                        ExternalImageSource::NativeTexture(eid) => {
                            panic!("Unexpected external texture {:?} for the texture cache update of {:?}", eid, id);
                        }
                    }
                }
                TextureUpdateSource::DebugClear => unreachable!(),
            };

            let upload_start_time = zeitstempel::now();

            stats.bytes_uploaded += renderer.texture_uploader.upload(
                &mut renderer.device,
                texture,
                rect,
                stride,
                format_override,
                data.as_ptr(),
                data.len()
            );

            stats.upload_time += zeitstempel::now() - upload_start_time;

            if let TextureUpdateSource::External { id, channel_index } = source {
                let handler = renderer.external_image_handler
                    .as_mut()
                    .expect("Found external image, but no handler set!");
                handler.unlock(id, channel_index);
            }
        }
    }

    // Fill and upload each staging texture in turn. We only have one staging
    // buffer mapped at a time, so this cannot be done until all of the regions
    // within each staging texture have been reserved.
    for batch_buffer in batch_upload_buffers.into_iter().map(|(_, (_, buffers))| buffers).flatten() {
        upload_staging_texture(
            &mut renderer.device,
            &mut renderer.texture_uploader,
            &mut renderer.staging_texture_pool,
            &mut renderer.external_image_handler,
            &batch_upload_textures[batch_buffer.texture_index],
            batch_buffer,
            &mut stats,
        );
    }

    if !batch_upload_copies.is_empty() {
        // Copy updates that were batch uploaded to their correct destination in the texture cache.
        // Sort them by destination and source to minimize framebuffer binding changes.
        batch_upload_copies.sort_unstable_by_key(|b| (b.dest_texture_id.0, b.src_texture_index));

        let gpu_copy_start = zeitstempel::now();

        if renderer.device.use_draw_calls_for_texture_copy() {
            // Some drivers have a very high CPU overhead when submitting hundreds of small blit
            // commands (low end intel drivers on Windows for example can take take 100+ ms submitting a
            // few hundred blits). In this case we do the copy with batched draw calls.
            copy_from_staging_to_cache_using_draw_calls(
                renderer,
                &mut stats,
                &batch_upload_textures,
                batch_upload_copies,
            );
        } else {
            copy_from_staging_to_cache(
                renderer,
                &batch_upload_textures,
                batch_upload_copies,
            );
        }

        stats.gpu_copy_commands_time += zeitstempel::now() - gpu_copy_start;
    }

    for texture in batch_upload_textures.drain(..) {
        renderer.staging_texture_pool.return_texture(texture);
    }

    // Update the profile counters. We use add instead of set because
    // this function can be called several times per frame.
    // We don't update the counters when their value is zero, so that
    // the profiler can treat them as events and we can get notified
    // when they happen.

    let upload_total = zeitstempel::now() - upload_total_start;
    renderer.profile.add(
        profiler::TOTAL_UPLOAD_TIME,
        profiler::ns_to_ms(upload_total)
    );

    if num_updates > 0 {
        renderer.profile.add(profiler::TEXTURE_UPLOADS, num_updates);
    }

    if stats.bytes_uploaded > 0 {
        renderer.profile.add(
            profiler::TEXTURE_UPLOADS_MEM,
            profiler::bytes_to_mb(stats.bytes_uploaded)
        );
    }

    if stats.cpu_copy_time > 0 {
        renderer.profile.add(
            profiler::UPLOAD_CPU_COPY_TIME,
            profiler::ns_to_ms(stats.cpu_copy_time)
        );
    }
    if stats.upload_time > 0 {
        renderer.profile.add(
            profiler::UPLOAD_TIME,
            profiler::ns_to_ms(stats.upload_time)
        );
    }
    if stats.texture_alloc_time > 0 {
        renderer.profile.add(
            profiler::STAGING_TEXTURE_ALLOCATION_TIME,
            profiler::ns_to_ms(stats.texture_alloc_time)
        );
    }
    if stats.cpu_buffer_alloc_time > 0 {
        renderer.profile.add(
            profiler::CPU_TEXTURE_ALLOCATION_TIME,
            profiler::ns_to_ms(stats.cpu_buffer_alloc_time)
        );
    }
    if stats.num_draw_calls > 0{
        renderer.profile.add(
            profiler::UPLOAD_NUM_COPY_BATCHES,
            stats.num_draw_calls
        );
    }

    if stats.gpu_copy_commands_time > 0 {
        renderer.profile.add(
            profiler::UPLOAD_GPU_COPY_TIME,
            profiler::ns_to_ms(stats.gpu_copy_commands_time)
        );
    }

    let add_markers = profiler::thread_is_being_profiled();
    if add_markers && stats.bytes_uploaded > 0 {
    	let details = format!("{} bytes uploaded, {} items", stats.bytes_uploaded, stats.items_uploaded);
    	profiler::add_text_marker(&"Texture uploads", &details, Duration::from_nanos(upload_total));
    }
}

/// Reserve a region within a batched upload staging texture for an item, and
/// record the item so that it can be copied there later.
fn reserve_staging_buffer_region(
    device: &mut Device,
    staging_texture_pool: &mut UploadTexturePool,
    update_rect: DeviceIntRect,
    source_offset: usize,
    source_stride: Option<i32>,
    source: TextureUpdateSource,
    dest_texture_id: CacheTextureId,
    format: ImageFormat,
    batch_upload_buffers: &mut FastHashMap<ImageFormat, (GuillotineAllocator, Vec<BatchUploadBuffer>)>,
    batch_upload_textures: &mut Vec<Texture>,
    batch_upload_copies: &mut Vec<BatchUploadCopy>,
    stats: &mut UploadStats
) {
    let (allocator, buffers) = batch_upload_buffers.entry(format)
        .or_insert_with(|| (GuillotineAllocator::new(None), Vec::new()));

    // Allocate a region within the staging buffer for this update. If there is
    // no room in an existing buffer then allocate another texture and buffer.
    let (slice, origin) = match allocator.allocate(&update_rect.size()) {
        Some((slice, origin)) => (slice, origin),
        None => {
            let new_slice = FreeRectSlice(buffers.len() as u32);
            allocator.extend(new_slice, BATCH_UPLOAD_TEXTURE_SIZE, update_rect.size());

            let texture_alloc_time_start = zeitstempel::now();
            let staging_texture = staging_texture_pool.get_texture(device, format);
            stats.texture_alloc_time += zeitstempel::now() - texture_alloc_time_start;

            let texture_index = batch_upload_textures.len();
            batch_upload_textures.push(staging_texture);

            buffers.push(BatchUploadBuffer {
                items: Vec::new(),
                texture_index,
                upload_rect: DeviceIntRect::zero()
            });

            (new_slice, DeviceIntPoint::zero())
        }
    };
    let buffer = &mut buffers[slice.0 as usize];
    let allocated_rect = DeviceIntRect::from_origin_and_size(origin, update_rect.size());
    buffer.upload_rect = buffer.upload_rect.union(&allocated_rect);

    batch_upload_copies.push(BatchUploadCopy {
        src_texture_index: buffer.texture_index,
        src_offset: allocated_rect.min,
        dest_texture_id,
        dest_offset: update_rect.min,
        size: update_rect.size(),
    });

    buffer.items.push(BatchUploadItem {
        source,
        source_offset,
        source_stride,
        dst_rect: allocated_rect,
    });
}

/// Take this code path instead of copying into a staging buffer when the image
/// we would copy is large enough that it's unlikely anything else would fit in the
/// buffer, therefore we might as well upload directly from the source image's pixels.
fn skip_staging_buffer(
    device: &mut Device,
    uploader: &mut TextureUploader,
    staging_texture_pool: &mut UploadTexturePool,
    update_rect: DeviceIntRect,
    data: &[u8],
    stride: Option<i32>,
    dest_texture_id: CacheTextureId,
    format: ImageFormat,
    batch_upload_textures: &mut Vec<Texture>,
    batch_upload_copies: &mut Vec<BatchUploadCopy>,
    stats: &mut UploadStats
) {
    let texture_alloc_time_start = zeitstempel::now();
    let staging_texture = staging_texture_pool.get_texture(device, format);
    stats.texture_alloc_time += zeitstempel::now() - texture_alloc_time_start;

    let upload_start_time = zeitstempel::now();
    stats.bytes_uploaded += uploader.upload(
        device,
        &staging_texture,
        DeviceIntRect::from_size(update_rect.size()),
        stride,
        None,
        data.as_ptr(),
        data.len(),
    );
    stats.upload_time += zeitstempel::now() - upload_start_time;

    let texture_index = batch_upload_textures.len();
    batch_upload_textures.push(staging_texture);

    batch_upload_copies.push(BatchUploadCopy {
        src_texture_index: texture_index,
        src_offset: DeviceIntPoint::zero(),
        dest_texture_id,
        dest_offset: update_rect.min,
        size: update_rect.size(),
    });
}

/// Copy each of a staging texture's items in to a staging buffer, then upload
/// the buffer to the staging texture.
///
/// Only the rectangle enclosing the items is uploaded, so that we do not
/// transfer the whole staging texture when it is only partially used.
fn upload_staging_texture(
    device: &mut Device,
    uploader: &mut TextureUploader,
    staging_texture_pool: &mut UploadTexturePool,
    external_image_handler: &mut Option<Box<dyn ExternalImageHandler>>,
    texture: &Texture,
    buffer: BatchUploadBuffer,
    stats: &mut UploadStats,
) {
    // The first region allocated within each staging texture is placed at the
    // origin, so the enclosing rectangle starts there too. The row offsets used
    // when filling the staging buffer rely on this.
    assert_eq!(buffer.upload_rect.min, DeviceIntPoint::zero());

    let format = texture.get_format();
    let bpp = format.bytes_per_pixel() as usize;

    if matches!(device.upload_method(), &UploadMethod::PixelBuffer(..)) {
        let stage_start_time = zeitstempel::now();
        let staged = uploader.stage(device, format, buffer.upload_rect.size());
        stats.upload_time += zeitstempel::now() - stage_start_time;

        // As with any other upload, if we cannot map a range of the PBO then the
        // upload is dropped.
        if let Ok(mut staged) = staged {
            let dst_stride = staged.stride();
            copy_items_into_staging_buffer(
                &buffer.items,
                staged.data_mut(),
                dst_stride,
                bpp,
                external_image_handler,
                stats,
            );

            let upload_start_time = zeitstempel::now();
            stats.bytes_uploaded += staged.upload(texture, buffer.upload_rect, None);
            stats.upload_time += zeitstempel::now() - upload_start_time;
        }

        return;
    }

    let cpu_buffer_alloc_start_time = zeitstempel::now();
    let mut bytes = staging_texture_pool.get_temporary_buffer();
    stats.cpu_buffer_alloc_time += zeitstempel::now() - cpu_buffer_alloc_start_time;

    // Device::begin_frame() sets GL_UNPACK_ALIGNMENT to 1, so a tight stride is fine.
    let dst_stride = buffer.upload_rect.width() as usize * bpp;
    copy_items_into_staging_buffer(
        &buffer.items,
        &mut bytes[..],
        dst_stride,
        bpp,
        external_image_handler,
        stats,
    );

    let upload_start_time = zeitstempel::now();
    stats.bytes_uploaded += uploader.upload(
        device,
        texture,
        buffer.upload_rect,
        Some(dst_stride as i32),
        None,
        bytes.as_ptr(),
        bytes.len(),
    );
    stats.upload_time += zeitstempel::now() - upload_start_time;

    staging_texture_pool.return_temporary_buffer(bytes);
}

/// Copy each item into the staging buffer which will be uploaded to a staging
/// texture. `dst` describes the staging texture's enclosing rectangle, whose
/// origin is the staging texture's origin, using `dst_stride`.
fn copy_items_into_staging_buffer(
    items: &[BatchUploadItem],
    dst: &mut [mem::MaybeUninit<u8>],
    dst_stride: usize,
    bpp: usize,
    external_image_handler: &mut Option<Box<dyn ExternalImageHandler>>,
    stats: &mut UploadStats,
) {
    for item in items {
        match item.source {
            TextureUpdateSource::Bytes { ref data } => {
                copy_item_rows(dst, dst_stride, bpp, item, &data[item.source_offset ..], stats);
            }
            TextureUpdateSource::External { id, channel_index } => {
                let handler = external_image_handler
                    .as_mut()
                    .expect("Found external image, but no handler set!");
                // The filter is only relevant for NativeTexture external images.
                match handler.lock(id, channel_index, false).source {
                    ExternalImageSource::RawData(data) => {
                        copy_item_rows(dst, dst_stride, bpp, item, &data[item.source_offset ..], stats);
                    }
                    ExternalImageSource::Invalid => {
                        // WR haven't support RGBAF32 format in texture_cache, so
                        // we use u8 type here.
                        fill_item_rows(dst, dst_stride, bpp, item, 0xff, stats);
                    }
                    ExternalImageSource::NativeTexture(eid) => {
                        panic!("Unexpected external texture {:?} for the texture cache update of {:?}", eid, id);
                    }
                }
                external_image_handler.as_mut().unwrap().unlock(id, channel_index);
            }
            TextureUpdateSource::DebugClear => unreachable!(),
        }
    }
}

fn copy_item_rows(
    dst: &mut [mem::MaybeUninit<u8>],
    dst_stride: usize,
    bpp: usize,
    item: &BatchUploadItem,
    src: &[u8],
    stats: &mut UploadStats,
) {
    let memcpy_start_time = zeitstempel::now();

    let width_bytes = item.dst_rect.width() as usize * bpp;
    let src_stride = item.source_stride.map_or(width_bytes, |stride| {
        assert!(stride >= 0);
        stride as usize
    });
    let src_size = (item.dst_rect.height() as usize - 1) * src_stride + width_bytes;
    assert!(src_size <= src.len());

    let src: &[mem::MaybeUninit<u8>] = unsafe {
        std::slice::from_raw_parts(src.as_ptr() as *const _, src_size)
    };

    // copy the data line-by-line in to the buffer so that we do not overwrite
    // any other region of the buffer.
    for y in 0..item.dst_rect.height() as usize {
        let src_start = y * src_stride;
        let src_end = src_start + width_bytes;
        let dst_start = (item.dst_rect.min.y as usize + y) * dst_stride +
            item.dst_rect.min.x as usize * bpp;
        let dst_end = dst_start + width_bytes;

        dst[dst_start..dst_end].copy_from_slice(&src[src_start..src_end])
    }

    stats.cpu_copy_time += zeitstempel::now() - memcpy_start_time;
}

fn fill_item_rows(
    dst: &mut [mem::MaybeUninit<u8>],
    dst_stride: usize,
    bpp: usize,
    item: &BatchUploadItem,
    value: u8,
    stats: &mut UploadStats,
) {
    let memcpy_start_time = zeitstempel::now();

    let width_bytes = item.dst_rect.width() as usize * bpp;
    for y in 0..item.dst_rect.height() as usize {
        let dst_start = (item.dst_rect.min.y as usize + y) * dst_stride +
            item.dst_rect.min.x as usize * bpp;
        let dst_end = dst_start + width_bytes;

        dst[dst_start..dst_end].fill(mem::MaybeUninit::new(value));
    }

    stats.cpu_copy_time += zeitstempel::now() - memcpy_start_time;
}

/// Copy from the staging PBOs or textures to texture cache textures using blit commands.
///
/// Using blits instead of draw calls is supposedly more efficient but some drivers have
/// a very high per-command overhead so in some configurations we end up using
/// copy_from_staging_to_cache_using_draw_calls instead.
fn copy_from_staging_to_cache(
    renderer: &mut Renderer,
    batch_upload_textures: &[Texture],
    batch_upload_copies: Vec<BatchUploadCopy>,
) {
    for copy in batch_upload_copies {
        let dest_texture = &renderer.texture_resolver.texture_cache_map[&copy.dest_texture_id].texture;

        renderer.device.copy_texture_sub_region(
            &batch_upload_textures[copy.src_texture_index],
            copy.src_offset.x as _,
            copy.src_offset.y as _,
            dest_texture,
            copy.dest_offset.x as _,
            copy.dest_offset.y as _,
            copy.size.width as _,
            copy.size.height as _,
        );
    }
}

/// Generate and submit composite shader batches to copy from
/// the staging textures to the destination cache textures.
///
/// If this shows up in GPU time ptofiles we could replace it with
/// a simpler shader (composite.glsl is already quite simple).
fn copy_from_staging_to_cache_using_draw_calls(
    renderer: &mut Renderer,
    stats: &mut UploadStats,
    batch_upload_textures: &[Texture],
    batch_upload_copies: Vec<BatchUploadCopy>,
) {
    let mut copy_instances = Vec::new();
    let mut prev_src = None;
    let mut prev_dst = None;
    let mut dst_texture_size = DeviceSize::new(0.0, 0.0);

    for copy in batch_upload_copies {

        let src_changed = prev_src != Some(copy.src_texture_index);
        let dst_changed = prev_dst != Some(copy.dest_texture_id);

        if (src_changed || dst_changed) && !copy_instances.is_empty() {
            renderer.draw_instanced_batch(
                &copy_instances,
                VertexArrayKind::Copy,
                // We bind the staging texture manually because it isn't known
                // to the texture resolver.
                &BatchTextures::empty(),
                &mut RendererStats::default(),
            );

            stats.num_draw_calls += 1;
            copy_instances.clear();
        }

        if dst_changed {
            let dest_texture = &renderer.texture_resolver.texture_cache_map[&copy.dest_texture_id].texture;
            dst_texture_size = dest_texture.get_dimensions().to_f32();

            let draw_target = DrawTarget::from_texture(dest_texture, false);
            renderer.device.bind_draw_target(draw_target);

            renderer.shaders
                .borrow_mut()
                .ps_copy()
                .bind(
                    &mut renderer.device,
                    &Transform3D::identity(),
                    None,
                    &mut renderer.renderer_errors,
                    &mut renderer.profile,
                    &mut renderer.command_log,
                );

            prev_dst = Some(copy.dest_texture_id);
        }

        if src_changed {
            renderer.device.bind_texture(
                TextureSampler::Color0,
                &batch_upload_textures[copy.src_texture_index],
                Swizzle::default(),
            );

            prev_src = Some(copy.src_texture_index)
        }

        let src_rect = DeviceRect::from_origin_and_size(
            copy.src_offset.to_f32(),
            copy.size.to_f32(),
        );

        let dst_rect = DeviceRect::from_origin_and_size(
            copy.dest_offset.to_f32(),
            copy.size.to_f32(),
        );

        copy_instances.push(CopyInstance {
            src_rect,
            dst_rect,
            dst_texture_size,
        });
    }

    if !copy_instances.is_empty() {
        renderer.draw_instanced_batch(
            &copy_instances,
            VertexArrayKind::Copy,
            &BatchTextures::empty(),
            &mut RendererStats::default(),
        );

        stats.num_draw_calls += 1;
    }
}

/// A very basic pool to avoid reallocating staging textures as well as staging
/// CPU side buffers.
pub struct UploadTexturePool {
    /// The textures in the pool associated with a last used frame index.
    ///
    /// The outer array corresponds to each of teh three supported texture formats.
    textures: [VecDeque<(Texture, u64)>; BATCH_UPLOAD_FORMAT_COUNT],
    // Frame at which to deallocate some textures if there are too many in the pool,
    // for each format.
    delay_texture_deallocation: [u64; BATCH_UPLOAD_FORMAT_COUNT],
    current_frame: u64,

    /// Temporary buffers that are used when using staging uploads + glTexImage2D.
    ///
    /// Temporary buffers aren't used asynchronously so they can be reused every frame.
    /// To keep things simple we always allocate enough memory for formats with four bytes
    /// per pixel (more than we need for alpha-only textures but it works just as well).
    temporary_buffers: Vec<Vec<mem::MaybeUninit<u8>>>,
    min_temporary_buffers: usize,
    delay_buffer_deallocation: u64,
}

impl UploadTexturePool {
    pub fn new() -> Self {
        UploadTexturePool {
            textures: [VecDeque::new(), VecDeque::new(), VecDeque::new(), VecDeque::new()],
            delay_texture_deallocation: [0; BATCH_UPLOAD_FORMAT_COUNT],
            current_frame: 0,
            temporary_buffers: Vec::new(),
            min_temporary_buffers: 0,
            delay_buffer_deallocation: 0,
        }
    }

    fn format_index(&self, format: ImageFormat) -> usize {
        match format {
            ImageFormat::RGBA8 => 0,
            ImageFormat::BGRA8 => 1,
            ImageFormat::R8 => 2,
            ImageFormat::R16 => 3,
            _ => { panic!("unexpected format {:?}", format); }
        }
    }

    pub fn begin_frame(&mut self) {
        self.current_frame += 1;
        self.min_temporary_buffers = self.temporary_buffers.len();
    }

    /// Create or reuse a staging texture.
    ///
    /// See also return_texture.
    pub fn get_texture(&mut self, device: &mut Device, format: ImageFormat) -> Texture {

        // First try to reuse a texture from the pool.
        // "available" here means hasn't been used for 2 frames to avoid stalls.
        // No need to scan the vector. Newer textures are always pushed at the back
        // of the vector so we know the first element is the least recently used.
        let format_idx = self.format_index(format);
        let can_reuse = self.textures[format_idx].get(0)
            .map(|tex| self.current_frame - tex.1 > 2)
            .unwrap_or(false);

        if can_reuse {
            return self.textures[format_idx].pop_front().unwrap().0;
        }

        // If we couldn't find an available texture, create a new one.

        device.create_texture(
            ImageBufferKind::Texture2D,
            format,
            BATCH_UPLOAD_TEXTURE_SIZE.width,
            BATCH_UPLOAD_TEXTURE_SIZE.height,
            TextureFilter::Nearest,
            // Currently we need render target support as we always use glBlitFramebuffer
            // to copy the texture data. Instead, we should use glCopyImageSubData on some
            // platforms, and avoid creating the FBOs in that case.
            Some(RenderTargetInfo { has_depth: false }),
        )
    }

    /// Hand the staging texture back to the pool after being done with uploads.
    ///
    /// The texture must have been obtained from this pool via get_texture.
    pub fn return_texture(&mut self, texture: Texture) {
        let format_idx = self.format_index(texture.get_format());
        self.textures[format_idx].push_back((texture, self.current_frame));
    }

    /// Create or reuse a temporary CPU buffer.
    ///
    /// These buffers are used in the batched upload path when PBOs are not supported.
    /// Content is first written to the temporary buffer and uploaded via a single
    /// glTexSubImage2D call.
    pub fn get_temporary_buffer(&mut self) -> Vec<mem::MaybeUninit<u8>> {
        let buffer = self.temporary_buffers.pop().unwrap_or_else(|| {
            vec![mem::MaybeUninit::new(0); BATCH_UPLOAD_TEXTURE_SIZE.area() as usize * 4]
        });
        self.min_temporary_buffers = self.min_temporary_buffers.min(self.temporary_buffers.len());
        buffer
    }

    /// Return memory that was obtained from this pool via get_temporary_buffer.
    pub fn return_temporary_buffer(&mut self, buffer: Vec<mem::MaybeUninit<u8>>) {
        assert_eq!(buffer.len(), BATCH_UPLOAD_TEXTURE_SIZE.area() as usize * 4);
        self.temporary_buffers.push(buffer);
    }

    /// Deallocate this pool's CPU and GPU memory.
    pub fn delete_textures(&mut self, device: &mut Device) {
        for format in &mut self.textures {
            while let Some(texture) = format.pop_back() {
                device.delete_texture(texture.0)
            }
        }
        self.temporary_buffers.clear();
    }

    /// Deallocate some textures if there are too many for a long time.
    pub fn end_frame(&mut self, device: &mut Device) {
        for format_idx in 0..self.textures.len() {
            // Count the number of reusable staging textures.
            // if it stays high for a large number of frames, truncate it back to 8-ish
            // over multiple frames.

            let mut num_reusable_textures = 0;
            for texture in &self.textures[format_idx] {
                if self.current_frame - texture.1 > 2 {
                    num_reusable_textures += 1;
                }
            }

            if num_reusable_textures < 8 {
                // Don't deallocate textures for another 120 frames.
                self.delay_texture_deallocation[format_idx] = self.current_frame + 120;
            }

            // Deallocate up to 4 staging textures every frame.
            let to_remove = if self.current_frame > self.delay_texture_deallocation[format_idx] {
                num_reusable_textures.min(4)
            } else {
                0
            };

            for _ in 0..to_remove {
                let texture = self.textures[format_idx].pop_front().unwrap().0;
                device.delete_texture(texture);
            }
        }

        // Similar logic for temporary CPU buffers. Our calls to get and return
        // temporary buffers should have been balanced for this frame, but the call
        // get_temporary_buffer will allocate a buffer if the vec is empty. Since we
        // carry these buffers from frame to frame, we keep track of the smallest
        // length of the temporary_buffers vec that we encountered this frame. Those
        // buffers were not touched and we deallocate some if there are a lot of them.
        let unused_buffers = self.min_temporary_buffers;
        if unused_buffers < 8 {
            self.delay_buffer_deallocation = self.current_frame + 120;
        }
        let to_remove = if self.current_frame > self.delay_buffer_deallocation  {
            unused_buffers.min(4)
        } else {
            0
        };
        for _ in 0..to_remove {
            // Unlike textures it doesn't matter whether we pop from the front or back
            // of the vector.
            self.temporary_buffers.pop();
        }
    }

    pub fn report_memory_to(&self, report: &mut MemoryReport, size_op_funs: &MallocSizeOfOps) {
        for buf in &self.temporary_buffers {
            report.upload_staging_memory += unsafe { (size_op_funs.size_of_op)(buf.as_ptr() as *const _) };
        }

        for format in &self.textures {
            for texture in format {
                report.upload_staging_textures += texture.0.size_in_bytes();
            }
        }
    }
}

struct UploadStats {
    num_draw_calls: u32,
    upload_time: u64,
    cpu_buffer_alloc_time: u64,
    texture_alloc_time: u64,
    cpu_copy_time: u64,
    gpu_copy_commands_time: u64,
    bytes_uploaded: usize,
    items_uploaded: usize,
}

/// An item which has been allocated a region within a staging texture, but whose
/// source has not yet been resolved.
#[derive(Debug)]
struct BatchUploadItem {
    source: TextureUpdateSource,
    source_offset: usize,
    source_stride: Option<i32>,
    // The region allocated for this item within the staging texture. Its
    // destination in the texture cache is described by the corresponding
    // BatchUploadCopy.
    dst_rect: DeviceIntRect,
}

#[derive(Debug)]
struct BatchUploadBuffer {
    items: Vec<BatchUploadItem>,
    texture_index: usize,
    // A rectangle containing all items going into this staging texture, so
    // that we can avoid uploading the entire area if we are using glTexSubImage2d.
    upload_rect: DeviceIntRect,
}

// On some devices performing many small texture uploads is slow, so instead we batch
// updates in to a small number of uploads to temporary textures, then copy from those
// textures to the correct place in the texture cache.
// A list of temporary textures that batches of updates are uploaded to.
#[derive(Debug)]
struct BatchUploadCopy {
    // Index within batch_upload_textures
    src_texture_index: usize,
    src_offset: DeviceIntPoint,
    dest_texture_id: CacheTextureId,
    dest_offset: DeviceIntPoint,
    size: DeviceIntSize,
}
