//! Toolkit-neutral appearance values distributed atomically by `touchbar-sessiond`.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorScheme {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MotionPolicy {
    #[default]
    Full,
    Reduced,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ColorRole {
    Background = 0,
    Surface = 1,
    SurfaceHover = 2,
    SurfacePressed = 3,
    Foreground = 4,
    Muted = 5,
    Accent = 6,
    Destructive = 7,
}

impl ColorRole {
    pub const ALL: [Self; 8] = [
        Self::Background,
        Self::Surface,
        Self::SurfaceHover,
        Self::SurfacePressed,
        Self::Foreground,
        Self::Muted,
        Self::Accent,
        Self::Destructive,
    ];

    pub fn from_raw(value: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|role| *role as u32 == value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8 {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Rgba8 {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha: 255,
        }
    }

    /// Encode as `0xRRGGBBAA`; the renderer performs premultiplication.
    pub const fn packed(self) -> u32 {
        u32::from_be_bytes([self.red, self.green, self.blue, self.alpha])
    }

    pub const fn from_packed(value: u32) -> Self {
        let [red, green, blue, alpha] = value.to_be_bytes();
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppearanceSnapshot {
    pub generation: u32,
    pub scheme: ColorScheme,
    pub motion: MotionPolicy,
    pub background: Rgba8,
    pub surface: Rgba8,
    pub surface_hover: Rgba8,
    pub surface_pressed: Rgba8,
    pub foreground: Rgba8,
    pub muted: Rgba8,
    pub accent: Rgba8,
    pub destructive: Rgba8,
    pub corner_radius_millipixels: u32,
}

impl AppearanceSnapshot {
    pub fn color(self, role: ColorRole) -> Rgba8 {
        match role {
            ColorRole::Background => self.background,
            ColorRole::Surface => self.surface,
            ColorRole::SurfaceHover => self.surface_hover,
            ColorRole::SurfacePressed => self.surface_pressed,
            ColorRole::Foreground => self.foreground,
            ColorRole::Muted => self.muted,
            ColorRole::Accent => self.accent,
            ColorRole::Destructive => self.destructive,
        }
    }

    pub fn set_color(&mut self, role: ColorRole, color: Rgba8) {
        match role {
            ColorRole::Background => self.background = color,
            ColorRole::Surface => self.surface = color,
            ColorRole::SurfaceHover => self.surface_hover = color,
            ColorRole::SurfacePressed => self.surface_pressed = color,
            ColorRole::Foreground => self.foreground = color,
            ColorRole::Muted => self.muted = color,
            ColorRole::Accent => self.accent = color,
            ColorRole::Destructive => self.destructive = color,
        }
    }
}

impl Default for AppearanceSnapshot {
    fn default() -> Self {
        Self {
            generation: 1,
            scheme: ColorScheme::Dark,
            motion: MotionPolicy::Full,
            background: Rgba8::rgb(2, 4, 3),
            surface: Rgba8::rgb(33, 33, 38),
            surface_hover: Rgba8::rgb(48, 48, 55),
            surface_pressed: Rgba8::rgb(61, 61, 70),
            foreground: Rgba8::rgb(245, 245, 247),
            muted: Rgba8::rgb(140, 140, 148),
            accent: Rgba8::rgb(107, 242, 64),
            destructive: Rgba8::rgb(242, 64, 64),
            corner_radius_millipixels: 9_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_rgba_round_trips_without_premultiplication() {
        let color = Rgba8 {
            red: 0x12,
            green: 0x34,
            blue: 0x56,
            alpha: 0x78,
        };
        assert_eq!(color.packed(), 0x1234_5678);
        assert_eq!(Rgba8::from_packed(color.packed()), color);
    }
}
