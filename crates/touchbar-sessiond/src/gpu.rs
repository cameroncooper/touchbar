use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem,
    os::fd::{AsRawFd, OwnedFd},
    ptr,
};

use anyhow::{Context as _, Result, anyhow, bail};
use glow::HasContext;
use khronos_egl as egl;
use touchbar_protocol::hardware_ipc::{HardwareSwapchain, OutputTransform};

use crate::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DmabufBufferData,
    DmabufPlane,
};

const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;
const EGL_PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31dd;
const EGL_SYNC_NATIVE_FENCE_ANDROID: egl::Enum = 0x3144;
const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: egl::Attrib = 0x3145;

// Linux UAPI: _IOWR('>', 4, struct sync_file_info). Keeping the definition
// local avoids accepting an arbitrary pollable fd merely because EGL defers
// validation until a later image import.
const SYNC_IOC_FILE_INFO: libc::c_ulong =
    ((3_u64 << 30) | ((mem::size_of::<SyncFileInfo>() as u64) << 16) | (b'>' as u64) << 8 | 4)
        as libc::c_ulong;

#[repr(C)]
#[derive(Default)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

const _: () = assert!(mem::size_of::<SyncFileInfo>() == 56);

type ImageTargetTexture = unsafe extern "system" fn(u32, *mut c_void);

pub struct CompletionFence(glow::Fence);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerGeometry {
    pub x: u32,
    pub width: u32,
    pub height: u32,
    pub opacity: f32,
    pub z_index: u32,
}

struct Layer {
    geometry: LayerGeometry,
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    ready: bool,
}

struct OutputBuffer {
    _fd: OwnedFd,
    image: egl::Image,
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
}

struct OutputSwapchain {
    info: HardwareSwapchain,
    transform: OutputTransform,
    buffers: Vec<OutputBuffer>,
}

pub struct GpuCompositor {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    gl: glow::Context,
    program: glow::Program,
    flip_uniform: Option<glow::UniformLocation>,
    opacity_uniform: Option<glow::UniformLocation>,
    transpose_uniform: Option<glow::UniformLocation>,
    scene_texture: glow::Texture,
    scene_framebuffer: glow::Framebuffer,
    image_target_texture: ImageTargetTexture,
    native_fence_sync: bool,
    layers: BTreeMap<u64, Layer>,
    output: Option<OutputSwapchain>,
    scratch: Vec<u8>,
    width: u32,
    height: u32,
    renderer_name: String,
    background_color: [f32; 4],
}

impl GpuCompositor {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        // SAFETY: libEGL is loaded through its stable ABI.
        let egl = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required().context("load libEGL.so.1")?
        };
        egl.bind_api(egl::OPENGL_ES_API)
            .context("select OpenGL ES API")?;

        // The session daemon is a headless compositor. Selecting EGL's default
        // display can bind it to X11 whenever DISPLAY is inherited, which in
        // turn creates an accidental dependency on the host /tmp/.X11-unix
        // socket and breaks PrivateTmp isolation. Mesa's surfaceless platform
        // selects the local render device without any window-system socket.
        // SAFETY: the surfaceless platform requires EGL_DEFAULT_DISPLAY and no
        // native-display object; the attribute list is correctly terminated.
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                egl::DEFAULT_DISPLAY,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("obtain surfaceless compositor EGL display")?;
        egl.initialize(display)
            .context("initialize compositor EGL")?;

        let extensions = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .context("query compositor EGL extensions")?
            .to_string_lossy();
        if !extensions
            .split_whitespace()
            .any(|ext| ext == "EGL_EXT_image_dma_buf_import")
        {
            bail!("EGL_EXT_image_dma_buf_import is unavailable");
        }
        let native_fence_sync = extensions
            .split_whitespace()
            .any(|extension| extension == "EGL_ANDROID_native_fence_sync");

        let config_attributes = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attributes)
            .context("choose compositor EGL config")?
            .context("no compositor GLES3 pbuffer EGL config")?;
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .context("create compositor GLES3 context")?;
        let surface = egl
            .create_pbuffer_surface(
                display,
                config,
                &[
                    egl::WIDTH,
                    width as i32,
                    egl::HEIGHT,
                    height as i32,
                    egl::NONE,
                ],
            )
            .context("create compositor EGL pbuffer")?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .context("make compositor GLES context current")?;

        // SAFETY: EGL owns the function pointers for the current context.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const () as *const c_void)
                    .unwrap_or(ptr::null())
            })
        };
        let image_target_texture = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .context("GL_OES_EGL_image is unavailable")?;
        // SAFETY: EGL returned this address for the named extension function.
        let image_target_texture = unsafe {
            std::mem::transmute::<unsafe extern "system" fn(), ImageTargetTexture>(
                image_target_texture,
            )
        };

        // SAFETY: all GL calls occur while the context above is current.
        let (
            program,
            flip_uniform,
            opacity_uniform,
            transpose_uniform,
            scene_texture,
            scene_framebuffer,
            renderer_name,
        ) = unsafe {
            let renderer_name = gl.get_parameter_string(glow::RENDERER);
            let program = compile_program(&gl)?;
            let flip_uniform = gl.get_uniform_location(program, "u_flip_y");
            let opacity_uniform = gl.get_uniform_location(program, "u_opacity");
            let transpose_uniform = gl.get_uniform_location(program, "u_transpose");
            let (scene_texture, scene_framebuffer) =
                create_color_target(&gl, width, height, "scene")?;
            (
                program,
                flip_uniform,
                opacity_uniform,
                transpose_uniform,
                scene_texture,
                scene_framebuffer,
                renderer_name,
            )
        };

        Ok(Self {
            egl,
            display,
            surface,
            context,
            gl,
            program,
            flip_uniform,
            opacity_uniform,
            transpose_uniform,
            scene_texture,
            scene_framebuffer,
            image_target_texture,
            native_fence_sync,
            layers: BTreeMap::new(),
            output: None,
            scratch: vec![0; width as usize * height as usize * 4],
            width,
            height,
            renderer_name,
            background_color: [0.0, 0.0, 0.0, 1.0],
        })
    }

    pub fn set_background_color(&mut self, rgba: [f32; 4]) {
        self.background_color = rgba;
    }

    pub fn renderer_name(&self) -> &str {
        &self.renderer_name
    }

    /// The most recent composited scene as top-down RGBA.
    ///
    /// Row 0 is the top scanline, not GL's bottom-left origin: every blit in
    /// this compositor maps framebuffer row 0 to texel row 0, so the scene
    /// keeps whatever row order the client buffers arrived in. Readback
    /// consumers must not flip.
    pub fn scene_pixels(&self) -> &[u8] {
        &self.scratch
    }

    pub fn install_output_swapchain(
        &mut self,
        info: HardwareSwapchain,
        buffers: Vec<OwnedFd>,
    ) -> Result<()> {
        let info = info.validate().context("validate ADP output swapchain")?;
        if info.format != DRM_FORMAT_XRGB8888 {
            bail!("ADP output requires XRGB8888 buffers");
        }
        if usize::from(info.buffer_count) != buffers.len() {
            bail!("ADP output descriptor count does not match its metadata");
        }
        if info.logical_height != self.height {
            bail!(
                "output logical height {} does not match the compositor scene height {}",
                info.logical_height,
                self.height
            );
        }
        // The presenter declares its physical scanout layout; the compositor
        // derives the transform rather than assuming one, so an already
        // landscape panel and a portrait one share this path.
        let transform = match info.transform() {
            Some(transform) => transform,
            None => bail!(
                "output physical size {}x{} is neither the logical size nor a 90-degree rotation of it",
                info.physical_width,
                info.physical_height
            ),
        };

        self.destroy_output_swapchain();
        // The panel decides the canvas. Resize before importing so the scene
        // spans the strip the presenter actually owns.
        self.resize_scene(info.logical_width)
            .context("resize composed scene to the attached panel")?;
        let mut imported = Vec::with_capacity(buffers.len());
        for fd in buffers {
            match self.import_output_buffer(info, fd) {
                Ok(buffer) => imported.push(buffer),
                Err(error) => {
                    self.destroy_output_buffers(imported);
                    return Err(error);
                }
            }
        }
        self.output = Some(OutputSwapchain {
            info,
            transform,
            buffers: imported,
        });
        Ok(())
    }

    pub fn output_buffer_count(&self) -> usize {
        self.output
            .as_ref()
            .map_or(0, |output| output.buffers.len())
    }

    pub fn remove_output_swapchain(&mut self) {
        self.destroy_output_swapchain();
    }

    pub fn render_scene_to_output(&self, index: usize) -> Result<()> {
        let output = self.output.as_ref().context("no ADP output is installed")?;
        let buffer = output
            .buffers
            .get(index)
            .context("ADP output buffer index is out of range")?;
        let allocation_width = output.info.pitch / 4;
        let logical_left = (output.info.logical_width - self.width) / 2;

        // SAFETY: the imported target and scene texture belong to this context.
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(buffer.framebuffer));
            self.gl.viewport(
                0,
                0,
                allocation_width as i32,
                output.info.physical_height as i32,
            );
            self.gl.clear_color(
                self.background_color[0],
                self.background_color[1],
                self.background_color[2],
                self.background_color[3],
            );
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            // Place the composed scene into the presenter's scanout buffer,
            // centering it when the panel is wider than the scene. The
            // transform is whatever the presenter declared, not an assumption.
            let transpose = match output.transform {
                OutputTransform::QuarterTurn => {
                    // Physical X is logical Y and physical Y is logical X, so
                    // the visible extent is the panel's narrow physical width
                    // and the scene runs along physical Y.
                    self.gl.viewport(
                        0,
                        logical_left as i32,
                        output.info.physical_width as i32,
                        self.width as i32,
                    );
                    true
                }
                OutputTransform::Identity => {
                    // Axes already agree; the scene maps straight across.
                    self.gl.viewport(
                        logical_left as i32,
                        0,
                        self.width as i32,
                        self.height as i32,
                    );
                    false
                }
            };
            self.gl.disable(glow::BLEND);
            // Match tiny-dfr's +90-degree logical-to-physical transform:
            // physical_x = height - 1 - logical_y, physical_y = logical_x.
            // Transposition alone mirrors the narrow axis and makes text
            // appear upside down on the installed panel, so the vertical flip
            // completes the rotation. It is part of that transform and not an
            // origin correction: the scene itself is already top-down, which
            // is what `scene_pixels` hands to readback consumers.
            self.draw_texture_transformed(self.scene_texture, true, 1.0, transpose);

            // Initial cross-device synchronization is explicit and simple.
            // This can become a native fence passed to the presenter later.
            self.gl.finish();
        }
        Ok(())
    }

    pub fn update_dmabuf_layer(
        &mut self,
        layer_id: u64,
        geometry: LayerGeometry,
        data: &DmabufBufferData,
    ) -> Result<CompletionFence> {
        if data.width != geometry.width || data.height != geometry.height || data.planes.len() != 1
        {
            bail!("DMA-BUF does not match its configured one-plane region");
        }
        if geometry.x.saturating_add(geometry.width) > self.width || geometry.height != self.height
        {
            bail!("layer geometry exceeds the compositor target");
        }

        let (layer_texture, layer_framebuffer) = self.ensure_layer(layer_id, geometry)?;
        let plane = &data.planes[0];
        let attributes = image_attributes(data, plane);
        // EGL_LINUX_DMA_BUF_EXT requires EGL_NO_CONTEXT and a null client buffer.
        // SAFETY: these null handles are exactly the sentinel values required by EGL.
        let no_context = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let no_buffer = unsafe { egl::ClientBuffer::from_ptr(ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                no_context,
                EGL_LINUX_DMA_BUF_EXT,
                no_buffer,
                &attributes,
            )
            .context("import plugin DMA-BUF as EGLImage")?;

        // SAFETY: the imported image and all GL objects belong to the current context.
        let imported_texture = unsafe {
            let texture = self
                .gl
                .create_texture()
                .map_err(|error| anyhow!("create imported texture: {error}"))?;
            configure_texture(&self.gl, texture);
            (self.image_target_texture)(glow::TEXTURE_2D, image.as_ptr());

            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(layer_framebuffer));
            self.gl
                .viewport(0, 0, geometry.width as i32, geometry.height as i32);
            self.gl.disable(glow::BLEND);
            self.draw_texture(texture, data.y_invert, 1.0);
            self.gl.flush();
            texture
        };

        if let Some(layer) = self.layers.get_mut(&layer_id) {
            layer.ready = true;
        }

        // Queue a GPU completion fence after the copy. The Wayland buffer can
        // be released asynchronously once this fence signals.
        let completion = unsafe {
            let fence = self
                .gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(|error| anyhow!("create GPU completion fence: {error}"))?;
            self.gl.flush();
            self.gl.delete_texture(imported_texture);
            CompletionFence(fence)
        };
        self.egl
            .destroy_image(self.display, image)
            .context("destroy imported EGLImage")?;

        // Keep the layer texture live; only the temporary imported texture is released.
        let _ = layer_texture;
        Ok(completion)
    }

    pub fn wait_on_acquire_fence(&self, fence: OwnedFd) -> Result<()> {
        if !self.native_fence_sync {
            bail!("plugin supplied an acquire fence but EGL native-fence import is unavailable");
        }
        validate_sync_file(&fence).context("validate plugin DMA-BUF acquire fence")?;
        let attributes = [
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID,
            fence.as_raw_fd() as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        // On success EGL takes ownership of the supplied sync_file. On error
        // `fence` remains owned here and closes normally.
        let sync = unsafe {
            self.egl
                .create_sync(self.display, EGL_SYNC_NATIVE_FENCE_ANDROID, &attributes)
        }
        .context("import plugin DMA-BUF acquire fence")?;
        std::mem::forget(fence);

        // eglWaitSync queues a server-side dependency in the compositor GLES
        // stream. It does not block this event thread waiting for the client.
        let wait = self
            .egl
            .wait_sync(self.display, sync, 0)
            .context("queue plugin DMA-BUF acquire fence wait");
        let destroy = unsafe { self.egl.destroy_sync(self.display, sync) }
            .context("destroy imported plugin acquire fence");
        wait?;
        destroy
    }

    pub fn update_rgba_layer(
        &mut self,
        layer_id: u64,
        geometry: LayerGeometry,
        pixels: &[u8],
    ) -> Result<()> {
        let expected = geometry.width as usize * geometry.height as usize * 4;
        if pixels.len() != expected {
            bail!("RGBA upload size does not match its configured region");
        }
        if geometry.x.saturating_add(geometry.width) > self.width || geometry.height != self.height
        {
            bail!("layer geometry exceeds the compositor target");
        }
        let (texture, _) = self.ensure_layer(layer_id, geometry)?;
        // SAFETY: the layer texture belongs to the current compositor context.
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                geometry.width as i32,
                geometry.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(pixels)),
            );
        }
        if let Some(layer) = self.layers.get_mut(&layer_id) {
            layer.ready = true;
        }
        Ok(())
    }

    pub fn compose_scene_gpu(&self) {
        // SAFETY: all objects belong to the current compositor context.
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.scene_framebuffer));
            self.gl
                .viewport(0, 0, self.width as i32, self.height as i32);
            self.gl.clear_color(
                self.background_color[0],
                self.background_color[1],
                self.background_color[2],
                self.background_color[3],
            );
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.enable(glow::BLEND);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

            let mut layers = self
                .layers
                .iter()
                .filter(|(_, layer)| layer.ready)
                .collect::<Vec<_>>();
            layers.sort_by_key(|(id, layer)| (layer.geometry.z_index, **id));
            for (_, layer) in layers {
                self.gl.viewport(
                    layer.geometry.x as i32,
                    0,
                    layer.geometry.width as i32,
                    layer.geometry.height as i32,
                );
                self.draw_texture(layer.texture, false, layer.geometry.opacity);
            }

            self.gl.disable(glow::BLEND);
            self.gl.flush();
        }
    }

    pub fn read_scene_checksum(&mut self) -> u64 {
        // SAFETY: the scene framebuffer belongs to the current context and
        // scratch has exactly enough space for the requested RGBA rectangle.
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.scene_framebuffer));
            self.gl.read_pixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut self.scratch)),
            );
            self.gl.finish();
        }
        checksum(&self.scratch)
    }

    pub fn compose_scene(&mut self) -> u64 {
        self.compose_scene_gpu();
        self.read_scene_checksum()
    }

    pub fn completion_signaled(&self, fence: &CompletionFence) -> Result<bool> {
        // A zero timeout only polls; it never stalls the compositor thread.
        let status = unsafe { self.gl.client_wait_sync(fence.0, 0, 0) };
        match status {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => Ok(true),
            glow::TIMEOUT_EXPIRED => Ok(false),
            glow::WAIT_FAILED => bail!("GPU completion fence wait failed"),
            other => bail!("unexpected GPU completion fence status {other:#x}"),
        }
    }

    pub fn destroy_completion(&self, fence: CompletionFence) {
        // SAFETY: this sync object belongs to the current compositor context.
        unsafe {
            self.gl.delete_sync(fence.0);
        }
    }

    pub fn finish(&self) {
        // Used only during orderly shutdown to retire all Wayland buffers.
        unsafe {
            self.gl.finish();
        }
    }

    pub fn remove_layer(&mut self, layer_id: u64) {
        let Some(layer) = self.layers.remove(&layer_id) else {
            return;
        };
        // SAFETY: the objects were created by this current GL context.
        unsafe {
            self.gl.delete_framebuffer(layer.framebuffer);
            self.gl.delete_texture(layer.texture);
        }
    }

    fn ensure_layer(
        &mut self,
        layer_id: u64,
        geometry: LayerGeometry,
    ) -> Result<(glow::Texture, glow::Framebuffer)> {
        if self.layers.get(&layer_id).is_some_and(|layer| {
            layer.geometry.width != geometry.width || layer.geometry.height != geometry.height
        }) {
            self.remove_layer(layer_id);
        }
        if let Some(layer) = self.layers.get_mut(&layer_id) {
            layer.geometry = geometry;
            return Ok((layer.texture, layer.framebuffer));
        }

        // SAFETY: this compositor context is current.
        let (texture, framebuffer) =
            unsafe { create_color_target(&self.gl, geometry.width, geometry.height, "layer")? };
        self.layers.insert(
            layer_id,
            Layer {
                geometry,
                texture,
                framebuffer,
                ready: false,
            },
        );
        Ok((texture, framebuffer))
    }

    unsafe fn draw_texture(&self, texture: glow::Texture, flip_y: bool, opacity: f32) {
        unsafe { self.draw_texture_transformed(texture, flip_y, opacity, false) }
    }

    unsafe fn draw_texture_transformed(
        &self,
        texture: glow::Texture,
        flip_y: bool,
        opacity: f32,
        transpose: bool,
    ) {
        // SAFETY: the caller guarantees that the compositor GL context is current.
        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl
                .uniform_1_i32(self.flip_uniform.as_ref(), i32::from(flip_y));
            self.gl
                .uniform_1_f32(self.opacity_uniform.as_ref(), opacity);
            self.gl
                .uniform_1_i32(self.transpose_uniform.as_ref(), i32::from(transpose));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    fn import_output_buffer(&self, info: HardwareSwapchain, fd: OwnedFd) -> Result<OutputBuffer> {
        let allocation_width = info.pitch / 4;
        let attributes = [
            egl::WIDTH as egl::Attrib,
            allocation_width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            info.physical_height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            info.format as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd.as_raw_fd() as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            0,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            info.pitch as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        // EGL_LINUX_DMA_BUF_EXT requires EGL_NO_CONTEXT and a null client buffer.
        // SAFETY: these null handles are exactly the sentinel values required by EGL.
        let no_context = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let no_buffer = unsafe { egl::ClientBuffer::from_ptr(ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                no_context,
                EGL_LINUX_DMA_BUF_EXT,
                no_buffer,
                &attributes,
            )
            .context("import ADP output DMA-BUF as EGLImage")?;

        // SAFETY: the EGLImage stays live and all GL objects use this context.
        let result = unsafe {
            let texture = match self.gl.create_texture() {
                Ok(texture) => texture,
                Err(error) => {
                    let _ = self.egl.destroy_image(self.display, image);
                    bail!("create ADP output texture: {error}");
                }
            };
            configure_texture(&self.gl, texture);
            (self.image_target_texture)(glow::TEXTURE_2D, image.as_ptr());
            let framebuffer = match self.gl.create_framebuffer() {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    self.gl.delete_texture(texture);
                    let _ = self.egl.destroy_image(self.display, image);
                    bail!("create ADP output framebuffer: {error}");
                }
            };
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                self.gl.delete_framebuffer(framebuffer);
                self.gl.delete_texture(texture);
                let _ = self.egl.destroy_image(self.display, image);
                bail!("ADP output framebuffer is incomplete: {status:#x}");
            }
            OutputBuffer {
                _fd: fd,
                image,
                texture,
                framebuffer,
            }
        };
        Ok(result)
    }

    fn destroy_output_buffers(&self, buffers: Vec<OutputBuffer>) {
        // SAFETY: the objects were created by this current GL/EGL context.
        unsafe {
            for buffer in buffers {
                self.gl.delete_framebuffer(buffer.framebuffer);
                self.gl.delete_texture(buffer.texture);
                let _ = self.egl.destroy_image(self.display, buffer.image);
            }
        }
    }

    /// The logical scene width this compositor composes into. It follows the
    /// attached panel rather than a compile-time constant, so a wider Touch
    /// Bar composes a wider scene instead of being letterboxed.
    pub fn canvas_width(&self) -> u32 {
        self.width
    }

    /// Resize the composed scene. Called when a presenter attaches a panel
    /// whose logical width differs from the current scene, which happens
    /// before any layer exists, so no layer geometry is invalidated.
    fn resize_scene(&mut self, width: u32) -> Result<()> {
        if width == self.width {
            return Ok(());
        }
        // SAFETY: the compositor GL context is current for the lifetime of
        // this struct, and both objects were created by it.
        let (texture, framebuffer) = unsafe {
            self.gl.delete_framebuffer(self.scene_framebuffer);
            self.gl.delete_texture(self.scene_texture);
            create_color_target(&self.gl, width, self.height, "scene")?
        };
        self.scene_texture = texture;
        self.scene_framebuffer = framebuffer;
        self.width = width;
        Ok(())
    }

    fn destroy_output_swapchain(&mut self) {
        if let Some(output) = self.output.take() {
            self.destroy_output_buffers(output.buffers);
        }
    }
}

fn validate_sync_file(fence: &OwnedFd) -> Result<()> {
    let mut info = SyncFileInfo::default();
    // SAFETY: SYNC_IOC_FILE_INFO reads and writes exactly one initialized
    // SyncFileInfo, and `fence` keeps the descriptor alive for the call.
    let result = unsafe { libc::ioctl(fence.as_raw_fd(), SYNC_IOC_FILE_INFO, &mut info) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("fd is not a Linux sync_file");
    }
    if info.num_fences == 0 {
        bail!("Linux sync_file contains no fences");
    }
    Ok(())
}

unsafe fn configure_texture(gl: &glow::Context, texture: glow::Texture) {
    // SAFETY: the caller guarantees that this GL context is current.
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
    }
}

unsafe fn create_color_target(
    gl: &glow::Context,
    width: u32,
    height: u32,
    label: &str,
) -> Result<(glow::Texture, glow::Framebuffer)> {
    // SAFETY: the caller guarantees that this GL context is current.
    unsafe {
        let texture = gl
            .create_texture()
            .map_err(|error| anyhow!("create {label} texture: {error}"))?;
        configure_texture(gl, texture);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width as i32,
            height as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        let framebuffer = match gl.create_framebuffer() {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                gl.delete_texture(texture);
                bail!("create {label} framebuffer: {error}");
            }
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(framebuffer);
            gl.delete_texture(texture);
            bail!("{label} framebuffer is incomplete");
        }
        Ok((texture, framebuffer))
    }
}

fn image_attributes(data: &DmabufBufferData, plane: &DmabufPlane) -> Vec<egl::Attrib> {
    let mut attributes = vec![
        egl::WIDTH as egl::Attrib,
        data.width as egl::Attrib,
        egl::HEIGHT as egl::Attrib,
        data.height as egl::Attrib,
        EGL_LINUX_DRM_FOURCC_EXT,
        data.format as egl::Attrib,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        plane.fd.as_raw_fd() as egl::Attrib,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        plane.offset as egl::Attrib,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        plane.stride as egl::Attrib,
    ];
    if plane.modifier != DRM_FORMAT_MOD_INVALID && plane.modifier != DRM_FORMAT_MOD_LINEAR {
        attributes.extend_from_slice(&[
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (plane.modifier & 0xffff_ffff) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (plane.modifier >> 32) as egl::Attrib,
        ]);
    } else if plane.modifier == DRM_FORMAT_MOD_LINEAR {
        attributes.extend_from_slice(&[
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            0,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            0,
        ]);
    }
    attributes.push(egl::ATTRIB_NONE);
    attributes
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn compile_program(gl: &glow::Context) -> Result<glow::Program> {
    const VERTEX: &str = r#"#version 300 es
precision highp float;
out vec2 v_uv;
void main() {
    vec2 positions[3] = vec2[3](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    vec2 uvs[3] = vec2[3](vec2(0.0, 0.0), vec2(2.0, 0.0), vec2(0.0, 2.0));
    gl_Position = vec4(positions[gl_VertexID], 0.0, 1.0);
    v_uv = uvs[gl_VertexID];
}
"#;
    const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_texture;
uniform int u_flip_y;
uniform float u_opacity;
uniform int u_transpose;
in vec2 v_uv;
out vec4 color;
void main() {
    vec2 uv = v_uv;
    if (u_transpose != 0) uv = uv.yx;
    if (u_flip_y != 0) uv.y = 1.0 - uv.y;
    color = texture(u_texture, uv) * u_opacity;
}
"#;

    // SAFETY: the caller has made this context current.
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX)?;
        let fragment = compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT)?;
        let program = gl
            .create_program()
            .map_err(|error| anyhow!("create compositor program: {error}"))?;
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            bail!("compositor shader link failed: {log}");
        }
        Ok(program)
    }
}

unsafe fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader> {
    // SAFETY: the caller guarantees that this GL context is current.
    unsafe {
        let shader = gl
            .create_shader(kind)
            .map_err(|error| anyhow!("create compositor shader: {error}"))?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            bail!("compositor shader compilation failed: {log}");
        }
        Ok(shader)
    }
}

impl Drop for GpuCompositor {
    fn drop(&mut self) {
        self.destroy_output_swapchain();
        // SAFETY: this context remains current and owns these objects.
        unsafe {
            for layer in std::mem::take(&mut self.layers).into_values() {
                self.gl.delete_framebuffer(layer.framebuffer);
                self.gl.delete_texture(layer.texture);
            }
            self.gl.delete_framebuffer(self.scene_framebuffer);
            self.gl.delete_texture(self.scene_texture);
            self.gl.delete_program(self.program);
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    use super::{SYNC_IOC_FILE_INFO, validate_sync_file};

    #[test]
    fn sync_file_ioctl_matches_linux_uapi() {
        assert_eq!(SYNC_IOC_FILE_INFO, 0xc038_3e04);
    }

    #[test]
    fn eventfd_is_not_an_acquire_fence() {
        // SAFETY: eventfd returns a new owned descriptor on success.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(raw >= 0);
        // SAFETY: ownership of the fresh descriptor transfers exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let error = validate_sync_file(&fd).expect_err("eventfd must not pass as sync_file");
        assert!(error.to_string().contains("fd is not a Linux sync_file"));
    }
}
