use touchbar_protocol::appearance::{AppearanceSnapshot, Rgba8};
use touchbar_ui::{Color, Image, SvgAsset, SvgRasterizer, TextAlign, TextEngine};

use crate::{ButtonVisual, SystemButton, SystemIcon};

pub struct SystemBarRenderer {
    text: TextEngine,
    icons: SvgRasterizer,
    pixels: Vec<u8>,
}

impl SystemBarRenderer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            text: TextEngine::new(),
            icons: SvgRasterizer::new(12),
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    /// The privileged fallback deliberately uses the same neutral visual
    /// language as Tiny DFR and Apple's control strip. It must remain legible
    /// even when no user theme service is running.
    pub fn render_platform(&mut self, buttons: &[SystemButton], width: u32, height: u32) -> &[u8] {
        self.render(buttons, platform_appearance(), width, height)
    }

    pub fn render(
        &mut self,
        buttons: &[SystemButton],
        appearance: AppearanceSnapshot,
        width: u32,
        height: u32,
    ) -> &[u8] {
        self.pixels.resize(width as usize * height as usize * 4, 0);
        fill(&mut self.pixels, appearance.background);
        for button in buttons {
            let background = if button.pressed {
                appearance.surface_pressed
            } else {
                appearance.surface
            };
            rounded_rect(
                &mut self.pixels,
                width,
                height,
                button.visual_bounds,
                appearance.corner_radius_millipixels as f32 / 1000.0,
                background,
            );
            let x = button.content_bounds.x.max(0.0).round() as u32;
            let y = button.content_bounds.y.max(0.0).round() as u32;
            let label_width = button.content_bounds.width.max(1.0).round() as u32;
            let label_height = button.content_bounds.height.max(1.0).round() as u32;
            match button.visual {
                ButtonVisual::Text(label) => {
                    let image = self
                        .text
                        .rasterize_bold(
                            label,
                            label_width,
                            label_height,
                            32.0,
                            color(appearance.foreground),
                            TextAlign::Center,
                        )
                        .clone();
                    composite(
                        &mut self.pixels,
                        width,
                        height,
                        x,
                        y,
                        &image.pixels,
                        image.width,
                        image.height,
                    );
                }
                ButtonVisual::Icon(icon) => {
                    let image = self.icon(icon);
                    let icon_x = x + label_width.saturating_sub(image.width) / 2;
                    let icon_y = y + label_height.saturating_sub(image.height) / 2;
                    composite_tinted(
                        &mut self.pixels,
                        width,
                        height,
                        icon_x,
                        icon_y,
                        &image,
                        appearance.foreground,
                    );
                }
            }
        }
        &self.pixels
    }

    fn icon(&mut self, icon: SystemIcon) -> Image {
        let asset = SvgAsset::new(icon as u64 + 1, 1, icon_source(icon))
            .expect("built-in system icon is a valid bounded SVG asset");
        self.icons
            .rasterize(&asset, 48, 48)
            .expect("built-in system icon rasterizes")
    }
}

fn icon_source(icon: SystemIcon) -> &'static str {
    match icon {
        SystemIcon::BrightnessDown => include_str!("../assets/brightness_low.svg"),
        SystemIcon::BrightnessUp => include_str!("../assets/brightness_high.svg"),
        SystemIcon::KeyboardIlluminationDown => include_str!("../assets/backlight_low.svg"),
        SystemIcon::KeyboardIlluminationUp => include_str!("../assets/backlight_high.svg"),
        SystemIcon::MicrophoneMute => include_str!("../assets/mic_off.svg"),
        SystemIcon::Search => include_str!("../assets/search.svg"),
        SystemIcon::Previous => include_str!("../assets/fast_rewind.svg"),
        SystemIcon::PlayPause => include_str!("../assets/play_pause.svg"),
        SystemIcon::Next => include_str!("../assets/fast_forward.svg"),
        SystemIcon::Mute => include_str!("../assets/volume_off.svg"),
        SystemIcon::VolumeDown => include_str!("../assets/volume_down.svg"),
        SystemIcon::VolumeUp => include_str!("../assets/volume_up.svg"),
    }
}

fn platform_appearance() -> AppearanceSnapshot {
    AppearanceSnapshot {
        background: Rgba8::rgb(0, 0, 0),
        surface: Rgba8::rgb(51, 51, 51),
        surface_hover: Rgba8::rgb(77, 77, 77),
        surface_pressed: Rgba8::rgb(102, 102, 102),
        foreground: Rgba8::rgb(255, 255, 255),
        corner_radius_millipixels: 8_000,
        ..AppearanceSnapshot::default()
    }
}

fn color(value: Rgba8) -> Color {
    Color::rgba(
        f32::from(value.red) / 255.0,
        f32::from(value.green) / 255.0,
        f32::from(value.blue) / 255.0,
        f32::from(value.alpha) / 255.0,
    )
}

fn fill(target: &mut [u8], color: Rgba8) {
    for pixel in target.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[color.red, color.green, color.blue, color.alpha]);
    }
}

fn rounded_rect(
    target: &mut [u8],
    width: u32,
    height: u32,
    rect: crate::Rect,
    radius: f32,
    color: Rgba8,
) {
    let left = rect.x.max(0.0).floor() as u32;
    let top = rect.y.max(0.0).floor() as u32;
    let right = (rect.x + rect.width).min(width as f32).ceil() as u32;
    let bottom = (rect.y + rect.height).min(height as f32).ceil() as u32;
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5).max(0.0);
    const SAMPLE_OFFSETS: [f32; 4] = [0.125, 0.375, 0.625, 0.875];
    for y in top..bottom {
        for x in left..right {
            let mut covered = 0_u32;
            for sample_y in SAMPLE_OFFSETS {
                for sample_x in SAMPLE_OFFSETS {
                    let px = x as f32 + sample_x;
                    let py = y as f32 + sample_y;
                    let nearest_x = px.clamp(rect.x + radius, rect.x + rect.width - radius);
                    let nearest_y = py.clamp(rect.y + radius, rect.y + rect.height - radius);
                    let dx = px - nearest_x;
                    let dy = py - nearest_y;
                    covered += u32::from(dx * dx + dy * dy <= radius * radius);
                }
            }
            if covered != 0 {
                let offset = (y as usize * width as usize + x as usize) * 4;
                blend_coverage(&mut target[offset..offset + 4], color, covered, 16);
            }
        }
    }
}

fn blend_coverage(target: &mut [u8], source: Rgba8, covered: u32, samples: u32) {
    let alpha = u32::from(source.alpha) * covered / samples;
    let inverse = 255 - alpha;
    for (channel, value) in [source.red, source.green, source.blue]
        .into_iter()
        .enumerate()
    {
        target[channel] =
            ((u32::from(value) * alpha + u32::from(target[channel]) * inverse + 127) / 255) as u8;
    }
    target[3] = 255;
}

#[allow(clippy::too_many_arguments)]
fn composite(
    target: &mut [u8],
    target_width: u32,
    target_height: u32,
    x: u32,
    y: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
) {
    for source_y in 0..source_height.min(target_height.saturating_sub(y)) {
        for source_x in 0..source_width.min(target_width.saturating_sub(x)) {
            let source_offset = (source_y as usize * source_width as usize + source_x as usize) * 4;
            let target_offset =
                ((y + source_y) as usize * target_width as usize + (x + source_x) as usize) * 4;
            let alpha = u32::from(source[source_offset + 3]);
            let inverse = 255 - alpha;
            for channel in 0..3 {
                target[target_offset + channel] = ((u32::from(source[source_offset + channel])
                    * alpha
                    + u32::from(target[target_offset + channel]) * inverse
                    + 127)
                    / 255) as u8;
            }
            target[target_offset + 3] = 255;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn composite_tinted(
    target: &mut [u8],
    target_width: u32,
    target_height: u32,
    x: u32,
    y: u32,
    source: &Image,
    tint: Rgba8,
) {
    for source_y in 0..source.height.min(target_height.saturating_sub(y)) {
        for source_x in 0..source.width.min(target_width.saturating_sub(x)) {
            let source_offset = (source_y as usize * source.width as usize + source_x as usize) * 4;
            let target_offset =
                ((y + source_y) as usize * target_width as usize + (x + source_x) as usize) * 4;
            let alpha = u32::from(source.pixels[source_offset + 3]) * u32::from(tint.alpha) / 255;
            let inverse = 255 - alpha;
            for (channel, value) in [tint.red, tint.green, tint.blue].into_iter().enumerate() {
                target[target_offset + channel] = ((u32::from(value) * alpha
                    + u32::from(target[target_offset + channel]) * inverse
                    + 127)
                    / 255) as u8;
            }
            target[target_offset + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SystemBar, SystemBarConfig};

    #[test]
    fn appearance_changes_the_rendered_scene() {
        let bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        let mut renderer = SystemBarRenderer::new(2008, 60);
        let first = renderer
            .render(&bar.buttons(), AppearanceSnapshot::default(), 2008, 60)
            .to_vec();
        let changed = AppearanceSnapshot {
            surface: Rgba8::rgb(120, 20, 30),
            ..AppearanceSnapshot::default()
        };
        let themed = renderer.render(&bar.buttons(), changed, 2008, 60).to_vec();
        assert_ne!(first, themed);
    }

    #[test]
    fn platform_style_is_black_with_tiny_dfr_button_levels() {
        let bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        let mut renderer = SystemBarRenderer::new(2008, 60);
        let pixels = renderer.render_platform(&bar.buttons(), 2008, 60);
        assert_eq!(&pixels[0..4], &[0, 0, 0, 255]);
        let first = bar.buttons()[0].visual_bounds;
        let offset = (30 * 2008 + first.x as usize + 10) * 4;
        assert_eq!(&pixels[offset..offset + 4], &[51, 51, 51, 255]);

        let square_corner = ((first.y as usize) * 2008 + first.x as usize) * 4;
        assert_eq!(&pixels[square_corner..square_corner + 4], &[0, 0, 0, 255]);
        let antialiased_corner = ((first.y as usize) * 2008 + first.x as usize + 6) * 4;
        assert!(pixels[antialiased_corner] > 0 && pixels[antialiased_corner] < 51);
    }
}
