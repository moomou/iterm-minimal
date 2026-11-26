//! Native Apple Metal renderer using `CAMetalLayer`, runtime MSL compilation,
//! instanced quad/glyph pipelines, incremental atlas uploads, and inline image textures.

use block2::RcBlock;
use iterm_render::{
    FrameBatches, GlyphAtlas, ViewportUniforms, TERMINAL_METAL_SHADER_SOURCE,
};
use iterm_vt::InlineImage;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLBuffer, MTLClearColor, MTLCommandBuffer,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLResourceOptions, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState, MTLSize, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use rustc_hash::{FxHashMap, FxHashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Live Metal GPU timing snapshot (microseconds) updated from `MTLCommandBuffer.GPUStartTime/GPUEndTime`.
#[derive(Debug, Default)]
pub struct GpuFrameStats {
    last_gpu_us: AtomicU64,
    ewma_gpu_us: AtomicU64,
    completed_frames: AtomicU64,
}

impl GpuFrameStats {
    fn record_gpu_sec(&self, secs: f64) {
        if !(secs.is_finite() && secs >= 0.0 && secs < 2.0) {
            return;
        }
        let us = (secs * 1_000_000.0).round() as u64;
        self.last_gpu_us.store(us, Ordering::Relaxed);
        let prev = self.ewma_gpu_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            us
        } else {
            (prev * 7 + us) / 8
        };
        self.ewma_gpu_us.store(next, Ordering::Relaxed);
        self.completed_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (f64, f64, u64) {
        (
            self.last_gpu_us.load(Ordering::Relaxed) as f64 / 1000.0,
            self.ewma_gpu_us.load(Ordering::Relaxed) as f64 / 1000.0,
            self.completed_frames.load(Ordering::Relaxed),
        )
    }
}

/// 3-permit GPU in-flight frame fence preventing CPU from overwriting `TripleRingBuffer`
/// slots while the GPU is still reading them!
struct InFlightFence {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl InFlightFence {
    fn new(max_in_flight: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Mutex::new(max_in_flight),
            cv: Condvar::new(),
        })
    }

    fn acquire(&self) -> bool {
        let mut guard = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + Duration::from_millis(50);
        while *guard == 0 {
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            if rem.is_zero() {
                return false;
            }
            let (g, res) = self.cv.wait_timeout(guard, rem).unwrap_or_else(|e| e.into_inner());
            guard = g;
            if res.timed_out() && *guard == 0 {
                return false;
            }
        }
        *guard -= 1;
        true
    }

    fn release(&self) {
        let mut guard = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        *guard = (*guard + 1).min(3);
        self.cv.notify_one();
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ImageUniforms {
    dst_rect: [f32; 4],
    uv_rect: [f32; 4],
    alpha: f32,
    _pad: [f32; 3],
}

/// Triple-buffered persistent `StorageModeShared` Metal buffer ring (zero GPU allocations per frame!).
struct TripleRingBuffer {
    slots: [Option<Retained<ProtocolObject<dyn MTLBuffer>>>; 3],
    capacities: [usize; 3],
}

impl Default for TripleRingBuffer {
    fn default() -> Self {
        Self {
            slots: [None, None, None],
            capacities: [0; 3],
        }
    }
}

impl TripleRingBuffer {
    /// Upload slice `items` into slot `slot_idx` (`frame_index % 3`), growing only when needed.
    fn upload<T: Copy>(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        slot_idx: usize,
        items: &[T],
    ) -> Option<&ProtocolObject<dyn MTLBuffer>> {
        let byte_len = std::mem::size_of_val(items);
        if byte_len == 0 {
            return None;
        }
        if self.capacities[slot_idx] < byte_len || self.slots[slot_idx].is_none() {
            let new_cap = (byte_len.next_power_of_two()).max(4096);
            let buf = device.newBufferWithLength_options(new_cap, MTLResourceOptions::StorageModeShared)?;
            self.slots[slot_idx] = Some(buf);
            self.capacities[slot_idx] = new_cap;
        }
        let buf = self.slots[slot_idx].as_ref()?;
        unsafe {
            let dst = buf.contents().as_ptr() as *mut u8;
            std::ptr::copy_nonoverlapping(items.as_ptr() as *const u8, dst, byte_len);
        }
        Some(&**buf)
    }
}

pub struct MetalRenderer {
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    quad_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    glyph_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    image_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas_sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    image_sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    mono_atlas_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    color_atlas_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    mono_atlas_epoch: u64,
    color_atlas_epoch: u64,
    image_textures: FxHashMap<u32, Retained<ProtocolObject<dyn MTLTexture>>>,
    bg_quad_ring: TripleRingBuffer,
    deco_quad_ring: TripleRingBuffer,
    overlay_quad_ring: TripleRingBuffer,
    glyph_ring: TripleRingBuffer,
    overlay_glyph_ring: TripleRingBuffer,
    in_flight: Arc<InFlightFence>,
    gpu_stats: Arc<GpuFrameStats>,
    frame_idx: usize,
}

impl MetalRenderer {
    pub fn new(atlas: &GlyphAtlas) -> Result<Self, String> {
        let device = MTLCreateSystemDefaultDevice()
            .ok_or_else(|| "MTLCreateSystemDefaultDevice returned nil".to_string())?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "newCommandQueue returned nil".to_string())?;

        // Compile MSL shader source at runtime on macOS (zero offline Xcode `metal` tool required!)
        let src = NSString::from_str(TERMINAL_METAL_SHADER_SOURCE);
        let library = device
            .newLibraryWithSource_options_error(&src, None)
            .map_err(|e| format!("Metal shader compilation failed: {e}"))?;

        let quad_pipeline = build_pipeline(&device, &library, "quad_vertex", "quad_fragment")?;
        let glyph_pipeline = build_pipeline(&device, &library, "glyph_vertex", "glyph_fragment")?;
        let image_pipeline = build_pipeline(&device, &library, "image_vertex", "image_fragment")?;

        let atlas_sampler = build_sampler(&device, MTLSamplerMinMagFilter::Linear)?;
        let image_sampler = build_sampler(&device, MTLSamplerMinMagFilter::Linear)?;

        let (mw, mh) = atlas.mono_dimensions();
        let (cw, ch) = atlas.color_dimensions();
        let mono_atlas_texture = create_texture_with_format(&device, MTLPixelFormat::R8Unorm, mw, mh)?;
        let color_atlas_texture =
            create_texture_with_format(&device, MTLPixelFormat::RGBA8Unorm, cw, ch)?;
        let (me, ce, _) = atlas.drain_uploads();

        Ok(Self {
            device,
            queue,
            quad_pipeline,
            glyph_pipeline,
            image_pipeline,
            atlas_sampler,
            image_sampler,
            mono_atlas_texture,
            color_atlas_texture,
            mono_atlas_epoch: me,
            color_atlas_epoch: ce,
            image_textures: FxHashMap::default(),
            bg_quad_ring: TripleRingBuffer::default(),
            deco_quad_ring: TripleRingBuffer::default(),
            overlay_quad_ring: TripleRingBuffer::default(),
            glyph_ring: TripleRingBuffer::default(),
            overlay_glyph_ring: TripleRingBuffer::default(),
            in_flight: InFlightFence::new(3),
            gpu_stats: Arc::new(GpuFrameStats::default()),
            frame_idx: 0,
        })
    }

    /// Return `(last_gpu_ms, ewma_gpu_ms, completed_frames)` measured directly on Metal GPU hardware.
    pub fn gpu_timing_ms(&self) -> (f64, f64, u64) {
        self.gpu_stats.snapshot()
    }

    pub fn configure_layer(&self, layer: &CAMetalLayer, opaque: bool) {
        layer.setDevice(Some(&self.device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);
        layer.setOpaque(opaque);
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(true);
    }

    #[inline]
    pub fn has_image_texture(&self, id: u32) -> bool {
        self.image_textures.contains_key(&id)
    }

    /// Release GPU textures for inline images no longer referenced by any live session.
    pub fn retain_image_textures(&mut self, live_ids: &FxHashSet<u32>) {
        self.image_textures.retain(|id, _| live_ids.contains(id));
    }

    /// Upload newly rasterized glyphs incrementally to the R8 mono or RGBA8 color atlas.
    pub fn sync_atlas(&mut self, atlas: &GlyphAtlas) {
        let (me, ce, uploads) = atlas.drain_uploads();
        if me != self.mono_atlas_epoch {
            let (mw, mh) = atlas.mono_dimensions();
            if let Ok(tex) = create_texture_with_format(&self.device, MTLPixelFormat::R8Unorm, mw, mh) {
                self.mono_atlas_texture = tex;
            }
            self.mono_atlas_epoch = me;
        }
        if ce != self.color_atlas_epoch {
            let (cw, ch) = atlas.color_dimensions();
            if let Ok(tex) =
                create_texture_with_format(&self.device, MTLPixelFormat::RGBA8Unorm, cw, ch)
            {
                self.color_atlas_texture = tex;
            }
            self.color_atlas_epoch = ce;
        }
        for up in uploads {
            if up.width == 0 || up.height == 0 {
                continue;
            }
            let region = MTLRegion {
                origin: MTLOrigin {
                    x: up.x as usize,
                    y: up.y as usize,
                    z: 0,
                },
                size: MTLSize {
                    width: up.width as usize,
                    height: up.height as usize,
                    depth: 1,
                },
            };
            let (tex, bpp) = if up.is_color {
                (&self.color_atlas_texture, 4usize)
            } else {
                (&self.mono_atlas_texture, 1usize)
            };
            unsafe {
                tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    NonNull::new_unchecked(up.pixels.as_ptr() as *mut _),
                    up.width as usize * bpp,
                );
            }
        }
    }

    /// Ensure an inline `imgcat` image is uploaded as an `MTLTexture`.
    fn ensure_image_texture(&mut self, img: &InlineImage) -> Option<&ProtocolObject<dyn MTLTexture>> {
        if !self.image_textures.contains_key(&img.id) {
            let tex = create_texture_with_format(
                &self.device,
                MTLPixelFormat::RGBA8Unorm,
                img.width_px,
                img.height_px,
            )
            .ok()?;
            let region = MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width: img.width_px as usize,
                    height: img.height_px as usize,
                    depth: 1,
                },
            };
            unsafe {
                tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    NonNull::new_unchecked(img.rgba.as_ptr() as *mut _),
                    (img.width_px * 4) as usize,
                );
            }
            self.image_textures.insert(img.id, tex);
        }
        self.image_textures.get(&img.id).map(|t| &**t)
    }

    /// Submit one frame of instanced backgrounds, decorations, text glyphs, inline images, and overlays.
    /// Returns `true` if a command buffer was committed, or `false` if skipped (caller should re-arm wakeup!).
    pub fn render_frame(
        &mut self,
        layer: &CAMetalLayer,
        atlas: &GlyphAtlas,
        batches: &FrameBatches,
        inline_images: &FxHashMap<u32, InlineImage>,
        viewport_w: f32,
        viewport_h: f32,
        scale_factor: f32,
        time_sec: f32,
        clear_rgba: [f64; 4],
    ) -> bool {
        // Gate on 3-permit GPU completion fence so TripleRingBuffer slot is guaranteed idle!
        if !self.in_flight.acquire() {
            return false;
        }
        let mut fence_committed = false;

        self.sync_atlas(atlas);

        for draw in &batches.images {
            if let Some(img) = inline_images.get(&draw.image_id) {
                let _ = self.ensure_image_texture(img);
            }
        }

        let slot = self.frame_idx % 3;
        self.frame_idx = self.frame_idx.wrapping_add(1);

        autoreleasepool(|_| {
            let Some(drawable) = layer.nextDrawable() else {
                return;
            };
            let Some(cmd_buf) = self.queue.commandBuffer() else {
                return;
            };

            let pass = MTLRenderPassDescriptor::renderPassDescriptor();
            unsafe {
                let color_att = pass.colorAttachments().objectAtIndexedSubscript(0);
                let drawable_tex = drawable.texture();
                color_att.setTexture(Some(&drawable_tex));
                color_att.setLoadAction(MTLLoadAction::Clear);
                color_att.setStoreAction(MTLStoreAction::Store);
                color_att.setClearColor(MTLClearColor {
                    red: clear_rgba[0],
                    green: clear_rgba[1],
                    blue: clear_rgba[2],
                    alpha: clear_rgba[3],
                });
            }

            let Some(enc) = cmd_buf.renderCommandEncoderWithDescriptor(&pass) else {
                return;
            };

            let uniforms = ViewportUniforms {
                viewport_size: [viewport_w.max(1.0), viewport_h.max(1.0)],
                scale_factor,
                time_sec,
            };

            // Pass 1: Backgrounds + focused block cursor background (zero-alloc ring buffer)
            if let Some(buf) = self.bg_quad_ring.upload(&self.device, slot, &batches.bg_quads) {
                encode_quads(&enc, &self.quad_pipeline, &uniforms, buf, batches.bg_quads.len());
            }

            // Pass 2: Inline images (behind text/cursor or inline in cell grid)
            if !batches.images.is_empty() {
                enc.setRenderPipelineState(&self.image_pipeline);
                unsafe {
                    enc.setVertexBytes_length_atIndex(
                        NonNull::from(&uniforms).cast(),
                        std::mem::size_of::<ViewportUniforms>(),
                        0,
                    );
                    enc.setFragmentSamplerState_atIndex(Some(&self.image_sampler), 0);
                }
                for draw in &batches.images {
                    if let Some(tex) = self.image_textures.get(&draw.image_id) {
                        let iu = ImageUniforms {
                            dst_rect: draw.dst_rect,
                            uv_rect: draw.uv_rect,
                            alpha: draw.alpha,
                            _pad: [0.0; 3],
                        };
                        unsafe {
                            enc.setVertexBytes_length_atIndex(
                                NonNull::from(&iu).cast(),
                                std::mem::size_of::<ImageUniforms>(),
                                1,
                            );
                            enc.setFragmentBytes_length_atIndex(
                                NonNull::from(&iu).cast(),
                                std::mem::size_of::<ImageUniforms>(),
                                1,
                            );
                            enc.setFragmentTexture_atIndex(Some(&**tex), 0);
                            enc.drawPrimitives_vertexStart_vertexCount(
                                MTLPrimitiveType::Triangle,
                                0,
                                6,
                            );
                        }
                    }
                }
            }

            // Pass 3: Instanced terminal text glyphs (R8 mono + RGBA8 color emoji in 1 draw call!)
            if let Some(buf) = self.glyph_ring.upload(&self.device, slot, &batches.glyphs) {
                encode_glyphs(
                    &enc,
                    &self.glyph_pipeline,
                    &self.mono_atlas_texture,
                    &self.color_atlas_texture,
                    &self.atlas_sampler,
                    &uniforms,
                    buf,
                    batches.glyphs.len(),
                );
            }

            // Pass 4: Underlines / curly underlines / strikethroughs / IME preedit underline
            if let Some(buf) = self.deco_quad_ring.upload(&self.device, slot, &batches.deco_quads) {
                encode_quads(&enc, &self.quad_pipeline, &uniforms, buf, batches.deco_quads.len());
            }

            // Pass 5: Non-block cursors, pane dividers, unified titlebar/tab bar, find overlay, visual bell
            if let Some(buf) = self.overlay_quad_ring.upload(&self.device, slot, &batches.overlay_quads) {
                encode_quads(
                    &enc,
                    &self.quad_pipeline,
                    &uniforms,
                    buf,
                    batches.overlay_quads.len(),
                );
            }

            // Pass 6: UI chrome text (tab titles, + button, Find query, status toasts)
            if let Some(buf) = self
                .overlay_glyph_ring
                .upload(&self.device, slot, &batches.overlay_glyphs)
            {
                encode_glyphs(
                    &enc,
                    &self.glyph_pipeline,
                    &self.mono_atlas_texture,
                    &self.color_atlas_texture,
                    &self.atlas_sampler,
                    &uniforms,
                    buf,
                    batches.overlay_glyphs.len(),
                );
            }

            enc.endEncoding();

            let fence = Arc::clone(&self.in_flight);
            let stats = Arc::clone(&self.gpu_stats);
            let completed_block = RcBlock::new(move |cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                let (t0, t1) = unsafe {
                    let c = cb.as_ref();
                    (c.GPUStartTime(), c.GPUEndTime())
                };
                if t1 > t0 {
                    stats.record_gpu_sec(t1 - t0);
                }
                fence.release();
            });
            unsafe {
                cmd_buf.addCompletedHandler(&*completed_block as *const _ as *mut _);
            }
            cmd_buf.presentDrawable(ProtocolObject::from_ref(&*drawable));
            cmd_buf.commit();
            fence_committed = true;
        });

        if !fence_committed {
            self.in_flight.release();
        }
        fence_committed
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_glyphs(
    enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    mono_atlas_texture: &ProtocolObject<dyn MTLTexture>,
    color_atlas_texture: &ProtocolObject<dyn MTLTexture>,
    atlas_sampler: &ProtocolObject<dyn MTLSamplerState>,
    uniforms: &ViewportUniforms,
    buf: &ProtocolObject<dyn MTLBuffer>,
    count: usize,
) {
    enc.setRenderPipelineState(pipeline);
    unsafe {
        enc.setVertexBytes_length_atIndex(
            NonNull::from(uniforms).cast(),
            std::mem::size_of::<ViewportUniforms>(),
            0,
        );
        enc.setVertexBuffer_offset_atIndex(Some(buf), 0, 1);
        enc.setFragmentTexture_atIndex(Some(mono_atlas_texture), 0);
        enc.setFragmentTexture_atIndex(Some(color_atlas_texture), 1);
        enc.setFragmentSamplerState_atIndex(Some(atlas_sampler), 0);
        enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
            MTLPrimitiveType::Triangle,
            0,
            6,
            count,
        );
    }
}

fn encode_quads(
    enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    uniforms: &ViewportUniforms,
    buf: &ProtocolObject<dyn MTLBuffer>,
    count: usize,
) {
    enc.setRenderPipelineState(pipeline);
    unsafe {
        enc.setVertexBytes_length_atIndex(
            NonNull::from(uniforms).cast(),
            std::mem::size_of::<ViewportUniforms>(),
            0,
        );
        enc.setVertexBuffer_offset_atIndex(Some(buf), 0, 1);
        enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
            MTLPrimitiveType::Triangle,
            0,
            6,
            count,
        );
    }
}

fn build_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    vert_name: &str,
    frag_name: &str,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = library
        .newFunctionWithName(&NSString::from_str(vert_name))
        .ok_or_else(|| format!("missing Metal vertex function {vert_name}"))?;
    let ffn = library
        .newFunctionWithName(&NSString::from_str(frag_name))
        .ok_or_else(|| format!("missing Metal fragment function {frag_name}"))?;

    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&vfn));
    desc.setFragmentFunction(Some(&ffn));
    unsafe {
        let att = desc.colorAttachments().objectAtIndexedSubscript(0);
        att.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        att.setBlendingEnabled(true);
        att.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
        att.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        att.setRgbBlendOperation(MTLBlendOperation::Add);
        att.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        att.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        att.setAlphaBlendOperation(MTLBlendOperation::Add);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| format!("failed to build pipeline {vert_name}/{frag_name}: {e}"))
}

fn build_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
    filter: MTLSamplerMinMagFilter,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let desc = MTLSamplerDescriptor::new();
    desc.setMinFilter(filter);
    desc.setMagFilter(filter);
    device
        .newSamplerStateWithDescriptor(&desc)
        .ok_or_else(|| "newSamplerStateWithDescriptor returned nil".to_string())
}

fn create_texture_with_format(
    device: &ProtocolObject<dyn MTLDevice>,
    pixel_format: MTLPixelFormat,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            pixel_format,
            width.max(1) as usize,
            height.max(1) as usize,
            false,
        )
    };
    desc.setUsage(MTLTextureUsage::ShaderRead);
    device
        .newTextureWithDescriptor(&desc)
        .ok_or_else(|| format!("failed to allocate {width}x{height} Metal texture"))
}
