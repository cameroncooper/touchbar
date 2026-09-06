//! Shared constants and generated custom Wayland protocol bindings.

pub const TOUCHBAR_PROTOCOL_VERSION: u32 = 1;

pub const DEFAULT_SOCKET_NAME: &str = "touchbar-0";

/// Logical canvas width used before a panel is attached, and by headless,
/// preview, and test paths that have no panel at all.
///
/// This is a starting value, not an invariant. A presenter that attaches a
/// swapchain declares the real logical width, and the compositor resizes its
/// scene to match, so a Touch Bar wider or narrower than the Apple silicon
/// panel composes at its own size rather than being letterboxed. Read the
/// live value from the compositor rather than assuming this constant.
pub const DEFAULT_REGION_WIDTH: u32 = 2008;

/// Logical canvas height. Fixed across every supported panel.
pub const TOUCHBAR_HEIGHT: u32 = 60;
pub const REFRESH_MILLIHZ: u32 = 60_000;

pub fn split_input_sequence(sequence: u64) -> Option<(u32, u32)> {
    (sequence != 0).then_some(((sequence >> 32) as u32, sequence as u32))
}

pub fn join_input_sequence(high: u32, low: u32) -> Option<u64> {
    let sequence = (u64::from(high) << 32) | u64::from(low);
    (sequence != 0).then_some(sequence)
}

#[cfg(test)]
mod sequence_tests {
    use super::*;

    #[test]
    fn nonzero_input_sequences_round_trip_across_wayland_words() {
        for sequence in [1, u64::from(u32::MAX), u64::from(u32::MAX) + 1, u64::MAX] {
            let (high, low) = split_input_sequence(sequence).unwrap();
            assert_eq!(join_input_sequence(high, low), Some(sequence));
        }
        assert_eq!(split_input_sequence(0), None);
        assert_eq!(join_input_sequence(0, 0), None);
    }

    #[test]
    fn frame_stream_uses_the_fresh_touchbar_magic() {
        assert_eq!(&FRAME_STREAM_MAGIC, b"TBARFRM1");
    }

    #[test]
    fn acquire_fence_protocol_errors_have_stable_v1_codes() {
        use server::touchbar_surface_v1::Error;

        assert_eq!(u32::from(Error::DuplicateAcquireFence), 0);
        assert_eq!(u32::from(Error::AcquireFenceWithoutBuffer), 1);
        assert_eq!(u32::from(Error::AcquireFenceUnsupportedBuffer), 2);
        assert_eq!(u32::from(Error::InvalidAcquireFence), 3);
    }
}

pub mod appearance;

// Shared-memory handoff from the unprivileged GPU compositor to the small
// privileged ADP presenter. A seqlock protects three tightly packed RGBA slots.
pub const FRAME_STREAM_MAGIC: [u8; 8] = *b"TBARFRM1";
pub const FRAME_STREAM_HEADER_SIZE: usize = 64;
pub const FRAME_STREAM_SLOT_COUNT: usize = 3;
pub const FRAME_STREAM_SEQUENCE_OFFSET: usize = 24;
pub const FRAME_STREAM_ACTIVE_SLOT_OFFSET: usize = 32;
pub const FRAME_STREAM_FLAGS_OFFSET: usize = 36;
pub const FRAME_STREAM_FLAG_BOTTOM_UP: u32 = 1;

#[cfg(target_os = "linux")]
pub mod hardware_ipc;

#[cfg(target_os = "linux")]
pub mod broker_ipc;

pub mod client {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/touchbar-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/touchbar-v1.xml");
}

pub mod server {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/touchbar-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("protocol/touchbar-v1.xml");
}
