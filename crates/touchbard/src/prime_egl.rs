use std::{
    ffi::{CStr, c_char, c_void},
    os::fd::{AsRawFd, BorrowedFd},
    ptr,
};

use anyhow::{Context as _, Result, anyhow, bail};
use glow::HasContext;
use khronos_egl as egl;

const EGL_PLATFORM_DEVICE_EXT: egl::Enum = 0x313f;
const EGL_DRM_DEVICE_FILE_EXT: egl::Int = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

type EglDevice = *mut c_void;
type QueryDevices =
    unsafe extern "system" fn(egl::Int, *mut EglDevice, *mut egl::Int) -> egl::Boolean;
type QueryDeviceString = unsafe extern "system" fn(EglDevice, egl::Int) -> *const c_char;
type ImageTargetTexture = unsafe extern "system" fn(u32, *mut c_void);

pub fn render_test(fd: BorrowedFd<'_>, size: (u32, u32), pitch: u32) -> Result<String> {
    // SAFETY: libEGL is loaded through its stable ABI.
    let egl = unsafe {
        egl::DynamicInstance::<egl::EGL1_5>::load_required().context("load libEGL.so.1")?
    };
    egl.bind_api(egl::OPENGL_ES_API)
        .context("select OpenGL ES for PRIME probe")?;
    let device = find_render_device(&egl)?;
    // SAFETY: EGL_EXT_platform_device defines EGLDeviceEXT as the native
    // display for this platform value.
    let display =
        unsafe { egl.get_platform_display(EGL_PLATFORM_DEVICE_EXT, device, &[egl::ATTRIB_NONE]) }
            .context("open EGL device platform")?;
    egl.initialize(display).context("initialize EGL device")?;

    let config = egl
        .choose_first_config(
            display,
            &[
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
                egl::NONE,
            ],
        )
        .context("choose PRIME probe EGL config")?
        .context("no EGL config for PRIME probe")?;
    let context = egl
        .create_context(
            display,
            config,
            None,
            &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
        )
        .context("create PRIME probe GLES context")?;
    let surface = egl
        .create_pbuffer_surface(display, config, &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE])
        .context("create PRIME probe pbuffer")?;
    egl.make_current(display, Some(surface), Some(surface), Some(context))
        .context("make PRIME probe context current")?;

    // SAFETY: the EGL context is current and owns these function pointers.
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            egl.get_proc_address(name)
                .map(|function| function as *const () as *const c_void)
                .unwrap_or(ptr::null())
        })
    };
    let image_target = egl
        .get_proc_address("glEGLImageTargetTexture2DOES")
        .context("GL_OES_EGL_image is unavailable")?;
    // SAFETY: EGL returned the address for this exact extension entry point.
    let image_target = unsafe {
        std::mem::transmute::<unsafe extern "system" fn(), ImageTargetTexture>(image_target)
    };
    let attributes = [
        egl::WIDTH as egl::Attrib,
        size.0 as egl::Attrib,
        egl::HEIGHT as egl::Attrib,
        size.1 as egl::Attrib,
        EGL_LINUX_DRM_FOURCC_EXT,
        DRM_FORMAT_XRGB8888 as egl::Attrib,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        fd.as_raw_fd() as egl::Attrib,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        0,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        pitch as egl::Attrib,
        egl::ATTRIB_NONE,
    ];
    // SAFETY: these null handles are the sentinels required for DMA-BUF image import.
    let no_context = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
    let no_buffer = unsafe { egl::ClientBuffer::from_ptr(ptr::null_mut()) };
    let image = egl
        .create_image(
            display,
            no_context,
            EGL_LINUX_DMA_BUF_EXT,
            no_buffer,
            &attributes,
        )
        .context("create EGL image from ADP DMA-BUF")?;

    // SAFETY: all GL objects belong to the current context and the EGL image
    // remains alive until after GPU completion.
    let renderer = unsafe {
        let renderer = gl.get_parameter_string(glow::RENDERER);
        let texture = gl
            .create_texture()
            .map_err(|error| anyhow!("create ADP PRIME texture: {error}"))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        image_target(glow::TEXTURE_2D, image.as_ptr());
        let framebuffer = gl
            .create_framebuffer()
            .map_err(|error| anyhow!("create ADP PRIME framebuffer: {error}"))?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            bail!("ADP PRIME framebuffer is incomplete: {status:#x}");
        }
        gl.viewport(0, 0, size.0 as i32, size.1 as i32);
        gl.clear_color(1.0, 0.5, 0.25, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.finish();
        gl.delete_framebuffer(framebuffer);
        gl.delete_texture(texture);
        renderer
    };

    egl.destroy_image(display, image)
        .context("destroy ADP PRIME EGL image")?;
    egl.make_current(display, None, None, None)
        .context("release PRIME probe context")?;
    egl.destroy_surface(display, surface)
        .context("destroy PRIME probe pbuffer")?;
    egl.destroy_context(display, context)
        .context("destroy PRIME probe context")?;
    egl.terminate(display)
        .context("terminate PRIME probe EGL")?;
    Ok(renderer)
}

fn find_render_device(egl: &egl::DynamicInstance<egl::EGL1_5>) -> Result<egl::NativeDisplayType> {
    let query_devices = egl
        .get_proc_address("eglQueryDevicesEXT")
        .context("EGL_EXT_device_enumeration is unavailable")?;
    let query_device_string = egl
        .get_proc_address("eglQueryDeviceStringEXT")
        .context("EGL_EXT_device_query is unavailable")?;
    // SAFETY: the addresses were returned for these exact extension functions.
    let query_devices =
        unsafe { std::mem::transmute::<unsafe extern "system" fn(), QueryDevices>(query_devices) };
    let query_device_string = unsafe {
        std::mem::transmute::<unsafe extern "system" fn(), QueryDeviceString>(query_device_string)
    };
    let mut devices = [ptr::null_mut(); 16];
    let mut count = 0;
    // SAFETY: both output arrays are valid for the supplied capacity.
    if unsafe { query_devices(devices.len() as egl::Int, devices.as_mut_ptr(), &mut count) }
        != egl::TRUE
    {
        bail!("eglQueryDevicesEXT failed");
    }
    for device in devices.into_iter().take(count as usize) {
        // Prefer a render node; the ADP card itself has no renderer.
        let render_node = unsafe { query_device_string(device, EGL_DRM_RENDER_NODE_FILE_EXT) };
        if !render_node.is_null() {
            return Ok(device);
        }
        let card_node = unsafe { query_device_string(device, EGL_DRM_DEVICE_FILE_EXT) };
        if !card_node.is_null() {
            let path = unsafe { CStr::from_ptr(card_node) }.to_string_lossy();
            if path.contains("renderD") {
                return Ok(device);
            }
        }
    }
    bail!("no EGL render device was found")
}
