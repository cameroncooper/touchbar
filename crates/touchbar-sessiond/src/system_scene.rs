use touchbar_protocol::{appearance::AppearanceSnapshot, hardware_ipc::TouchEvent};
use touchbar_system_bar::{KeyTransition, SystemBar, SystemBarConfig, SystemBarRenderer};

pub const LAYER_ID: u64 = u64::MAX;

/// The session daemon owns interaction state, while the shared renderer keeps
/// the themed session scene and the hardware daemon's emergency scene visually
/// and geometrically consistent.
pub struct SystemScene {
    bar: SystemBar,
    renderer: SystemBarRenderer,
}

impl SystemScene {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            bar: SystemBar::new(SystemBarConfig::default(), width as f32, height as f32),
            renderer: SystemBarRenderer::new(width, height),
        }
    }

    pub fn set_fn_pressed(&mut self, pressed: bool) -> (bool, Vec<KeyTransition>) {
        let previous = self.bar.active_layer();
        let transitions = self.bar.set_fn_pressed(pressed);
        (previous != self.bar.active_layer(), transitions)
    }

    pub fn handle_touch(&mut self, event: TouchEvent) -> Vec<KeyTransition> {
        self.bar.handle_touch(event)
    }

    pub fn cancel_all(&mut self) -> Vec<KeyTransition> {
        self.bar.cancel_all()
    }

    pub fn render(&mut self, appearance: AppearanceSnapshot, width: u32, height: u32) -> &[u8] {
        self.renderer
            .render(&self.bar.buttons(), appearance, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use touchbar_protocol::appearance::Rgba8;

    #[test]
    fn appearance_and_fn_layer_change_the_rendered_scene() {
        let mut scene = SystemScene::new(2008, 60);
        let first = scene
            .render(AppearanceSnapshot::default(), 2008, 60)
            .to_vec();
        scene.set_fn_pressed(true);
        let function = scene
            .render(AppearanceSnapshot::default(), 2008, 60)
            .to_vec();
        assert_ne!(first, function);

        let changed = AppearanceSnapshot {
            surface: Rgba8::rgb(120, 20, 30),
            ..AppearanceSnapshot::default()
        };
        let themed = scene.render(changed, 2008, 60).to_vec();
        assert_ne!(function, themed);
    }
}
