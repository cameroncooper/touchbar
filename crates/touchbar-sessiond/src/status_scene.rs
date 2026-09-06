use touchbar_protocol::appearance::{AppearanceSnapshot, Rgba8};
use touchbar_ui::{Color, TextAlign, TextEngine};

pub const LAYER_ID: u64 = u64::MAX - 1;
pub const WIDTH: u32 = 760;

pub struct StatusScene {
    text: TextEngine,
    pixels: Vec<u8>,
}

impl StatusScene {
    pub fn new(height: u32) -> Self {
        Self {
            text: TextEngine::new(),
            pixels: vec![0; WIDTH as usize * height as usize * 4],
        }
    }

    pub fn render(
        &mut self,
        item: &str,
        message: &str,
        appearance: AppearanceSnapshot,
        height: u32,
    ) -> &[u8] {
        self.pixels.resize(WIDTH as usize * height as usize * 4, 0);
        self.pixels.fill(0);
        let top = 7;
        let pill_height = height.saturating_sub(14);
        rounded_rect(
            &mut self.pixels,
            WIDTH,
            height,
            top,
            pill_height,
            appearance.corner_radius_millipixels / 1_000,
            appearance.surface,
        );
        let label = format!("{message} · {item}");
        let image = self
            .text
            .rasterize_bold(
                &label,
                WIDTH - 52,
                pill_height,
                18.0,
                color(appearance.foreground),
                TextAlign::Leading,
            )
            .clone();
        composite(
            &mut self.pixels,
            WIDTH,
            height,
            40,
            top,
            &image.pixels,
            image.width,
            image.height,
        );
        circle(
            &mut self.pixels,
            WIDTH,
            height,
            22,
            height / 2,
            5,
            appearance.destructive,
        );
        &self.pixels
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

#[allow(clippy::too_many_arguments)]
fn rounded_rect(
    target: &mut [u8],
    width: u32,
    height: u32,
    top: u32,
    rect_height: u32,
    radius: u32,
    color: Rgba8,
) {
    let left = 4_u32;
    let right = width.saturating_sub(4);
    let bottom = top.saturating_add(rect_height).min(height);
    let radius = radius.min(rect_height / 2).min((right - left) / 2);
    for y in top..bottom {
        for x in left..right {
            let nearest_x = x.clamp(left + radius, right.saturating_sub(radius + 1));
            let nearest_y = y.clamp(top + radius, bottom.saturating_sub(radius + 1));
            let dx = i64::from(x) - i64::from(nearest_x);
            let dy = i64::from(y) - i64::from(nearest_y);
            if dx * dx + dy * dy <= i64::from(radius * radius) {
                set_pixel(target, width, x, y, color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn circle(
    target: &mut [u8],
    width: u32,
    height: u32,
    center_x: u32,
    center_y: u32,
    radius: u32,
    color: Rgba8,
) {
    for y in center_y.saturating_sub(radius)..(center_y + radius + 1).min(height) {
        for x in center_x.saturating_sub(radius)..(center_x + radius + 1).min(width) {
            let dx = i64::from(x) - i64::from(center_x);
            let dy = i64::from(y) - i64::from(center_y);
            if dx * dx + dy * dy <= i64::from(radius * radius) {
                set_pixel(target, width, x, y, color);
            }
        }
    }
}

fn set_pixel(target: &mut [u8], width: u32, x: u32, y: u32, color: Rgba8) {
    let offset = (y as usize * width as usize + x as usize) * 4;
    target[offset..offset + 4].copy_from_slice(&[color.red, color.green, color.blue, 255]);
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
                ((y + source_y) as usize * target_width as usize + x as usize + source_x as usize)
                    * 4;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_transparent_outside_and_theme_colored_inside() {
        let appearance = AppearanceSnapshot::default();
        let mut scene = StatusScene::new(60);
        let pixels = scene.render(
            "github:owner/repo#item",
            "Plugin needs permission",
            appearance,
            60,
        );
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
        let center = (30 * WIDTH as usize + 22) * 4;
        assert_eq!(
            &pixels[center..center + 4],
            &[
                appearance.destructive.red,
                appearance.destructive.green,
                appearance.destructive.blue,
                255,
            ]
        );
    }
}
