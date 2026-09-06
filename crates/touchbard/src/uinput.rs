use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
};

use anyhow::{Context, Result, bail};
use input_linux::{EventKind, Key, SynchronizeKind, uinput::UInputHandle};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use touchbar_protocol::hardware_ipc::{KeyPhase, SystemKey};

const DEVICE_NAME: &[u8] = b"TouchBar System Keys";

/// The only input device `touchbard` exposes. It advertises exactly the keys
/// represented by `SystemKey`, rather than every Linux keycode.
pub struct VirtualKeyboard {
    handle: UInputHandle<File>,
    pressed: BTreeSet<SystemKey>,
}

impl VirtualKeyboard {
    pub fn open() -> Result<Self> {
        Self::open_at("/dev/uinput")
    }

    fn open_at(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let handle = UInputHandle::new(file);
        handle
            .set_evbit(EventKind::Key)
            .context("enable uinput keys")?;
        for key in all_keys() {
            handle
                .set_keybit(linux_key(key))
                .with_context(|| format!("enable uinput key {key:?}"))?;
        }

        let mut name = [0; 80];
        for (target, source) in name.iter_mut().zip(DEVICE_NAME.iter().copied()) {
            *target = source as _;
        }
        handle
            .dev_setup(&uinput_setup {
                id: input_id {
                    bustype: 0x19,
                    vendor: 0x1209,
                    product: 0x5442,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            })
            .context("configure Touch Bar virtual keyboard")?;
        handle
            .dev_create()
            .context("create Touch Bar virtual keyboard")?;
        Ok(Self {
            handle,
            pressed: BTreeSet::new(),
        })
    }

    pub fn emit(&mut self, key: SystemKey, phase: KeyPhase) -> Result<()> {
        let value = match phase {
            KeyPhase::Pressed if self.pressed.insert(key) => 1,
            KeyPhase::Pressed => return Ok(()),
            KeyPhase::Repeat if self.pressed.contains(&key) => 2,
            KeyPhase::Repeat => bail!("cannot repeat a system key that is not pressed"),
            KeyPhase::Released if self.pressed.remove(&key) => 0,
            KeyPhase::Released => return Ok(()),
        };
        self.write(linux_key(key), value)
    }

    pub fn release_all(&mut self) -> Result<()> {
        let keys = std::mem::take(&mut self.pressed);
        for key in keys {
            self.write(linux_key(key), 0)?;
        }
        Ok(())
    }

    fn write(&mut self, key: Key, value: i32) -> Result<()> {
        let written = self
            .handle
            .write(&[
                event(EventKind::Key, key as u16, value),
                event(EventKind::Synchronize, SynchronizeKind::Report as u16, 0),
            ])
            .with_context(|| format!("emit virtual key {key:?} value {value}"))?;
        if written != 2 {
            bail!("uinput accepted only {written} of 2 input events");
        }
        Ok(())
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.handle.dev_destroy();
    }
}

fn event(kind: EventKind, code: u16, value: i32) -> input_event {
    input_event {
        time: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: kind as u16,
        code,
        value,
    }
}

fn linux_key(key: SystemKey) -> Key {
    match key {
        SystemKey::BrightnessDown => Key::BrightnessDown,
        SystemKey::BrightnessUp => Key::BrightnessUp,
        SystemKey::KeyboardIlluminationDown => Key::IllumDown,
        SystemKey::KeyboardIlluminationUp => Key::IllumUp,
        SystemKey::PreviousSong => Key::PreviousSong,
        SystemKey::PlayPause => Key::PlayPause,
        SystemKey::NextSong => Key::NextSong,
        SystemKey::Mute => Key::Mute,
        SystemKey::VolumeDown => Key::VolumeDown,
        SystemKey::VolumeUp => Key::VolumeUp,
        SystemKey::MicrophoneMute => Key::MicMute,
        SystemKey::Search => Key::Search,
        SystemKey::F1 => Key::F1,
        SystemKey::F2 => Key::F2,
        SystemKey::F3 => Key::F3,
        SystemKey::F4 => Key::F4,
        SystemKey::F5 => Key::F5,
        SystemKey::F6 => Key::F6,
        SystemKey::F7 => Key::F7,
        SystemKey::F8 => Key::F8,
        SystemKey::F9 => Key::F9,
        SystemKey::F10 => Key::F10,
        SystemKey::F11 => Key::F11,
        SystemKey::F12 => Key::F12,
    }
}

const fn all_keys() -> [SystemKey; 24] {
    [
        SystemKey::BrightnessDown,
        SystemKey::BrightnessUp,
        SystemKey::KeyboardIlluminationDown,
        SystemKey::KeyboardIlluminationUp,
        SystemKey::PreviousSong,
        SystemKey::PlayPause,
        SystemKey::NextSong,
        SystemKey::Mute,
        SystemKey::VolumeDown,
        SystemKey::VolumeUp,
        SystemKey::MicrophoneMute,
        SystemKey::Search,
        SystemKey::F1,
        SystemKey::F2,
        SystemKey::F3,
        SystemKey::F4,
        SystemKey::F5,
        SystemKey::F6,
        SystemKey::F7,
        SystemKey::F8,
        SystemKey::F9,
        SystemKey::F10,
        SystemKey::F11,
        SystemKey::F12,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_protocol_key_maps_to_a_unique_linux_key() {
        let mapped = all_keys().map(linux_key);
        assert_eq!(mapped.into_iter().collect::<BTreeSet<_>>().len(), 24);
    }

    #[test]
    fn virtual_keyboard_name_fits_the_kernel_abi() {
        assert!(DEVICE_NAME.len() < 80);
    }
}
