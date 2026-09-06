use std::{thread, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use glow::HasContext as _;
use touchbar_client::{
    AppearanceSnapshot, Application, ClientOptions, FrameFlow, FrameInfo, Graphics, Rgba8, Sizing,
    SurfaceConfig, run,
};

struct ShaderDemo {
    program: Option<glow::Program>,
    time_uniform: Option<glow::UniformLocation>,
    resolution_uniform: Option<glow::UniformLocation>,
    variant_uniform: Option<glow::UniformLocation>,
    background_uniform: Option<glow::UniformLocation>,
    accent_uniform: Option<glow::UniformLocation>,
    foreground_uniform: Option<glow::UniformLocation>,
    appearance: AppearanceSnapshot,
    max_frames: u64,
    variant: f32,
    delay: Duration,
    plugin_id: String,
    animate: bool,
}

impl Application for ShaderDemo {
    fn configured(&mut self, graphics: &Graphics, config: SurfaceConfig) -> Result<()> {
        if self.program.is_none() {
            let gl = graphics.gl();
            // SAFETY: the SDK guarantees a current GLES context for callbacks.
            let program = unsafe { compile_program(gl)? };
            // SAFETY: `program` was linked in this current context.
            unsafe {
                self.time_uniform = gl.get_uniform_location(program, "u_time");
                self.resolution_uniform = gl.get_uniform_location(program, "u_resolution");
                self.variant_uniform = gl.get_uniform_location(program, "u_variant");
                self.background_uniform = gl.get_uniform_location(program, "u_background");
                self.accent_uniform = gl.get_uniform_location(program, "u_accent");
                self.foreground_uniform = gl.get_uniform_location(program, "u_foreground");
            }
            self.program = Some(program);
        }
        println!(
            "configured plugin={} region={}x{} renderer={} transport=dmabuf variant={}",
            self.plugin_id,
            config.width,
            config.height,
            graphics.renderer_name(),
            self.variant
        );
        Ok(())
    }

    fn appearance_changed(&mut self, appearance: AppearanceSnapshot) -> Result<bool> {
        self.appearance = appearance;
        println!(
            "shader-appearance generation={} scheme={:?} background=#{:08x} accent=#{:08x}",
            appearance.generation,
            appearance.scheme,
            appearance.background.packed(),
            appearance.accent.packed()
        );
        Ok(true)
    }

    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow> {
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        let gl = graphics.gl();
        let program = self.program.context("shader is not configured")?;
        // Keep deterministic frame-based animation for acceptance checks even
        // when the slow-client test intentionally delays one process.
        let time_seconds = frame.number as f32 / 60.0;
        // SAFETY: the SDK invokes rendering while its GLES context is current.
        unsafe {
            gl.viewport(
                0,
                0,
                frame.surface.width as i32,
                frame.surface.height as i32,
            );
            gl.use_program(Some(program));
            gl.uniform_1_f32(self.time_uniform.as_ref(), time_seconds);
            gl.uniform_2_f32(
                self.resolution_uniform.as_ref(),
                frame.surface.width as f32,
                frame.surface.height as f32,
            );
            gl.uniform_1_f32(self.variant_uniform.as_ref(), self.variant);
            set_rgb_uniform(
                gl,
                self.background_uniform.as_ref(),
                self.appearance.background,
            );
            set_rgb_uniform(gl, self.accent_uniform.as_ref(), self.appearance.accent);
            set_rgb_uniform(
                gl,
                self.foreground_uniform.as_ref(),
                self.appearance.foreground,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        Ok(next_frame_flow(self.animate, frame.number, self.max_frames))
    }
}

fn next_frame_flow(animate: bool, frame_number: u64, max_frames: u64) -> FrameFlow {
    if !animate {
        FrameFlow::Wait
    } else if frame_number + 1 >= max_frames {
        FrameFlow::Exit
    } else {
        FrameFlow::Animate
    }
}

unsafe fn set_rgb_uniform(
    gl: &glow::Context,
    uniform: Option<&glow::UniformLocation>,
    color: Rgba8,
) {
    // SAFETY: the caller has made the program containing `uniform` current.
    unsafe {
        gl.uniform_3_f32(
            uniform,
            f32::from(color.red) / 255.0,
            f32::from(color.green) / 255.0,
            f32::from(color.blue) / 255.0,
        );
    }
}

unsafe fn compile_program(gl: &glow::Context) -> Result<glow::Program> {
    const VERTEX: &str = r#"#version 300 es
precision highp float;
void main() {
    vec2 positions[3] = vec2[3](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    gl_Position = vec4(positions[gl_VertexID], 0.0, 1.0);
}
"#;
    const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform float u_time;
uniform vec2 u_resolution;
uniform float u_variant;
uniform vec3 u_background;
uniform vec3 u_accent;
uniform vec3 u_foreground;
out vec4 color;
void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    vec2 p = (gl_FragCoord.xy - 0.5 * u_resolution) / u_resolution.y;
    float wave = sin(p.x * 10.0 - u_time * (2.5 + 0.4 * u_variant));
    wave += 0.55 * sin(p.x * 21.0 + u_time * 1.7 + u_variant);
    wave += 0.25 * sin(p.x * 43.0 - u_time * 3.1);
    float ribbon = exp(-18.0 * abs(p.y - 0.12 * wave));
    float pulse = 0.55 + 0.45 * sin(u_time * 2.0 + uv.x * 6.28318);
    vec3 secondary = mix(u_accent, u_foreground, 0.28 + 0.22 * u_variant);
    vec3 ribbon_color = mix(u_accent, secondary, pulse);
    vec3 themed = mix(u_background, ribbon_color, clamp(ribbon * 0.88, 0.0, 1.0));
    float center_glow = 0.10 / (1.0 + 35.0 * dot(p, p));
    themed = mix(themed, u_accent, center_glow);
    color = vec4(themed, 1.0);
}
"#;

    // SAFETY: the caller guarantees that this GL context is current.
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX)?;
        let fragment = compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT)?;
        let program = gl
            .create_program()
            .map_err(|error| anyhow!("create shader program: {error}"))?;
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
            bail!("shader link failed: {log}");
        }
        Ok(program)
    }
}

unsafe fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader> {
    // SAFETY: the caller guarantees that this GL context is current.
    unsafe {
        let shader = gl
            .create_shader(kind)
            .map_err(|error| anyhow!("create shader: {error}"))?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            bail!("shader compilation failed: {log}");
        }
        Ok(shader)
    }
}

fn parse_args() -> (
    u64,
    bool,
    String,
    Option<String>,
    f32,
    Duration,
    Option<u32>,
    bool,
    bool,
) {
    let mut frames = 600_u64;
    let mut require_hardware = false;
    let mut plugin_id = "touchbar.gles-demo".to_string();
    let mut item_id = None;
    let mut variant = 0.0_f32;
    let mut delay_ms = 0_u64;
    let mut fixed_width = None;
    let mut backdrop = false;
    let mut animate = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .expect("--frames requires a value")
                    .parse()
                    .expect("--frames must be an integer");
            }
            "--require-hardware" => require_hardware = true,
            "--plugin-id" => plugin_id = args.next().expect("--plugin-id requires a value"),
            "--item-id" => item_id = Some(args.next().expect("--item-id requires a value")),
            "--variant" => {
                variant = args
                    .next()
                    .expect("--variant requires a value")
                    .parse()
                    .expect("--variant must be a number");
            }
            "--delay-ms" => {
                delay_ms = args
                    .next()
                    .expect("--delay-ms requires a value")
                    .parse()
                    .expect("--delay-ms must be an integer");
            }
            "--fixed-width" => {
                fixed_width = Some(
                    args.next()
                        .expect("--fixed-width requires a value")
                        .parse()
                        .expect("--fixed-width must be an integer"),
                );
            }
            "--backdrop" => backdrop = true,
            "--static" => animate = false,
            "--help" | "-h" => {
                println!(
                    "usage: touchbar-gl-demo [--frames N] [--static] [--require-hardware] [--plugin-id ID] [--item-id ID] [--variant 0..1] [--delay-ms N] [--fixed-width N] [--backdrop]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    assert!(frames > 0, "--frames must be greater than zero");
    (
        frames,
        require_hardware,
        plugin_id,
        item_id,
        variant.clamp(0.0, 1.0),
        Duration::from_millis(delay_ms),
        fixed_width,
        backdrop,
        animate,
    )
}

fn main() -> Result<()> {
    let (
        max_frames,
        require_hardware,
        plugin_id,
        item_id,
        variant,
        delay,
        fixed_width,
        backdrop,
        animate,
    ) = parse_args();
    let mut options = ClientOptions::new(&plugin_id).require_hardware(require_hardware);
    if let Some(item_id) = item_id {
        options = options.item_id(item_id);
    }
    if backdrop {
        options = options.backdrop();
    }
    if let Some(width) = fixed_width {
        options = options.compact_sizing(Sizing::new(width, width, width));
    }
    let summary = run(
        options,
        ShaderDemo {
            program: None,
            time_uniform: None,
            resolution_uniform: None,
            variant_uniform: None,
            background_uniform: None,
            accent_uniform: None,
            foreground_uniform: None,
            appearance: AppearanceSnapshot::default(),
            max_frames,
            variant,
            delay,
            plugin_id,
            animate,
        },
    )?;
    println!(
        "client-summary plugin={} frames={} transport=dmabuf",
        summary.plugin_id, summary.frames
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_scene_renders_once_and_then_idles() {
        assert!(matches!(next_frame_flow(false, 0, 1), FrameFlow::Wait));
        assert!(matches!(
            next_frame_flow(false, 999, 1_000),
            FrameFlow::Wait
        ));
    }

    #[test]
    fn animated_scene_stops_at_its_frame_limit() {
        assert!(matches!(next_frame_flow(true, 0, 2), FrameFlow::Animate));
        assert!(matches!(next_frame_flow(true, 1, 2), FrameFlow::Exit));
    }
}
