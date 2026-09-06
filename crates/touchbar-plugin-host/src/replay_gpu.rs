use std::{
    ffi::c_void, fs::OpenOptions, io::BufWriter, os::unix::fs::OpenOptionsExt, path::Path, ptr,
    time::Duration,
};

use anyhow::{Context, Result};
use glow::HasContext as _;
use khronos_egl as egl;
use touchbar_ui::{MotionPolicy, Scene, gles};

const EGL_PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31dd;

pub struct Rasterizer {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    gl: glow::Context,
    renderer: Option<gles::Renderer>,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    renderer_name: String,
}

impl Rasterizer {
    pub fn new(width: u32, height: u32, motion: MotionPolicy) -> Result<Self> {
        // SAFETY: libEGL is loaded through its stable ABI.
        let egl = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required().context("load libEGL.so.1")?
        };
        egl.bind_api(egl::OPENGL_ES_API)
            .context("select OpenGL ES API for replay screenshots")?;
        // SAFETY: Mesa's surfaceless platform takes EGL_DEFAULT_DISPLAY and
        // no native-display object; the attribute list is terminated.
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                egl::DEFAULT_DISPLAY,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("obtain surfaceless replay EGL display")?;
        egl.initialize(display)
            .context("initialize replay EGL display")?;
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
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .context("choose replay EGL config")?
            .context("no GLES3 RGBA pbuffer config is available")?;
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .context("create replay GLES3 context")?;
        let surface = match egl.create_pbuffer_surface(
            display,
            config,
            &[
                egl::WIDTH,
                width as i32,
                egl::HEIGHT,
                height as i32,
                egl::NONE,
            ],
        ) {
            Ok(surface) => surface,
            Err(error) => {
                let _ = egl.destroy_context(display, context);
                let _ = egl.terminate(display);
                return Err(error).context("create replay EGL pbuffer");
            }
        };
        if let Err(error) = egl.make_current(display, Some(surface), Some(surface), Some(context)) {
            let _ = egl.destroy_surface(display, surface);
            let _ = egl.destroy_context(display, context);
            let _ = egl.terminate(display);
            return Err(error).context("make replay GLES3 context current");
        }
        // SAFETY: EGL owns the function pointers for the current context.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const () as *const c_void)
                    .unwrap_or(ptr::null())
            })
        };
        let renderer_name = unsafe { gl.get_parameter_string(glow::RENDERER) };
        let renderer = match gles::Renderer::new(&gl) {
            Ok(renderer) => renderer,
            Err(error) => {
                let _ = egl.make_current(display, None, None, None);
                let _ = egl.destroy_surface(display, surface);
                let _ = egl.destroy_context(display, context);
                let _ = egl.terminate(display);
                return Err(error).context("create replay UI renderer");
            }
        };
        renderer.set_motion_policy(motion);
        Ok(Self {
            egl,
            display,
            surface,
            context,
            gl,
            renderer: Some(renderer),
            pixels: vec![0; width as usize * height as usize * 4],
            width,
            height,
            renderer_name,
        })
    }

    pub fn renderer_name(&self) -> &str {
        &self.renderer_name
    }

    pub fn set_motion_policy(&self, policy: MotionPolicy) {
        self.renderer
            .as_ref()
            .expect("renderer lives until Rasterizer::drop")
            .set_motion_policy(policy);
    }

    pub fn write_png(&mut self, scene: &Scene, now: Duration, path: &Path) -> Result<()> {
        self.renderer
            .as_ref()
            .expect("renderer lives until Rasterizer::drop")
            .draw_at(&self.gl, scene, self.width, self.height, now)?;
        // SAFETY: the pbuffer is current and pixels exactly fits one RGBA frame.
        unsafe {
            self.gl.read_pixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut self.pixels)),
            );
            self.gl.finish();
        }
        let stride = self.width as usize * 4;
        let mut top_down = vec![0_u8; self.pixels.len()];
        for y in 0..self.height as usize {
            let source = (self.height as usize - 1 - y) * stride;
            let target = y * stride;
            top_down[target..target + stride]
                .copy_from_slice(&self.pixels[source..source + stride]);
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create replay screenshot {}", path.display()))?;
        let writer = BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("write screenshot PNG header")?;
        writer
            .write_image_data(&top_down)
            .context("write screenshot PNG pixels")?;
        Ok(())
    }
}

impl Drop for Rasterizer {
    fn drop(&mut self) {
        let _ = self.renderer.take();
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}
