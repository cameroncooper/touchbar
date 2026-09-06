//! Privilege-boundary protocol between `touchbard` and `touchbar-sessiond`.
//!
//! The system daemon keeps hardware ownership and lends only PRIME DMA-BUF
//! descriptors to the active user session. Message types are directional so
//! neither side can accidentally accept authority intended for the other.

use std::{
    io, mem,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    ptr,
};

pub const SWAPCHAIN_MAGIC: [u8; 8] = *b"TBARHWD1";
pub const SWAPCHAIN_VERSION: u16 = 1;
pub const SWAPCHAIN_HEADER_SIZE: usize = 48;
pub const MIN_SWAPCHAIN_BUFFERS: usize = 2;
pub const MAX_SWAPCHAIN_BUFFERS: usize = 3;
pub const MESSAGE_MAGIC: [u8; 8] = *b"TBARMSG1";
pub const MESSAGE_VERSION: u16 = 1;
pub const MESSAGE_SIZE: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchPhase {
    Down,
    Motion,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TouchEvent {
    pub phase: TouchPhase,
    pub contact_id: u32,
    pub time_ms: u32,
    pub x_millipixels: i32,
    pub y_millipixels: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionMessage {
    FrameReady { index: u16, sequence: u64 },
    Key { key: SystemKey, phase: KeyPhase },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareMessage {
    BufferReleased { index: u16, sequence: u64 },
    Touch(TouchEvent),
    FnChanged { pressed: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPhase {
    Released,
    Pressed,
    Repeat,
}

/// The complete key-injection authority exposed by the v1 hardware daemon.
///
/// Protocol values deliberately are not Linux keycodes. `touchbard` performs
/// the only mapping to evdev, keeping arbitrary input injection impossible.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u16)]
pub enum SystemKey {
    BrightnessDown = 1,
    BrightnessUp = 2,
    KeyboardIlluminationDown = 3,
    KeyboardIlluminationUp = 4,
    PreviousSong = 5,
    PlayPause = 6,
    NextSong = 7,
    Mute = 8,
    VolumeDown = 9,
    VolumeUp = 10,
    MicrophoneMute = 11,
    Search = 12,
    F1 = 21,
    F2 = 22,
    F3 = 23,
    F4 = 24,
    F5 = 25,
    F6 = 26,
    F7 = 27,
    F8 = 28,
    F9 = 29,
    F10 = 30,
    F11 = 31,
    F12 = 32,
}

impl SystemKey {
    fn decode(value: u32) -> io::Result<Self> {
        Ok(match value {
            1 => Self::BrightnessDown,
            2 => Self::BrightnessUp,
            3 => Self::KeyboardIlluminationDown,
            4 => Self::KeyboardIlluminationUp,
            5 => Self::PreviousSong,
            6 => Self::PlayPause,
            7 => Self::NextSong,
            8 => Self::Mute,
            9 => Self::VolumeDown,
            10 => Self::VolumeUp,
            11 => Self::MicrophoneMute,
            12 => Self::Search,
            21 => Self::F1,
            22 => Self::F2,
            23 => Self::F3,
            24 => Self::F4,
            25 => Self::F5,
            26 => Self::F6,
            27 => Self::F7,
            28 => Self::F8,
            29 => Self::F9,
            30 => Self::F10,
            31 => Self::F11,
            32 => Self::F12,
            _ => return Err(invalid_data("unknown system key")),
        })
    }
}

/// Mapping from the compositor's logical landscape scene to the presenter's
/// physical scanout buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputTransform {
    /// Physical and logical axes agree; the scene is scanned out as composed.
    Identity,
    /// Physical X is logical Y and physical Y is logical X.
    QuarterTurn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareSwapchain {
    pub logical_width: u32,
    pub logical_height: u32,
    pub physical_width: u32,
    pub physical_height: u32,
    pub format: u32,
    pub pitch: u32,
    pub buffer_size: u64,
    pub buffer_count: u16,
}

impl HardwareSwapchain {
    pub fn validate(self) -> io::Result<Self> {
        if !(MIN_SWAPCHAIN_BUFFERS..=MAX_SWAPCHAIN_BUFFERS).contains(&(self.buffer_count as usize))
        {
            return Err(invalid_data("swapchain must contain two or three buffers"));
        }
        if self.logical_width == 0
            || self.logical_height == 0
            || self.physical_width == 0
            || self.physical_height == 0
        {
            return Err(invalid_data("swapchain dimensions must be nonzero"));
        }
        if self.logical_width > 16_384
            || self.logical_height > 16_384
            || self.physical_width > 16_384
            || self.physical_height > 16_384
        {
            return Err(invalid_data("swapchain dimensions exceed protocol limits"));
        }
        let minimum_pitch = self
            .physical_width
            .checked_mul(4)
            .ok_or_else(|| invalid_data("swapchain pitch overflow"))?;
        if self.pitch < minimum_pitch {
            return Err(invalid_data("swapchain pitch is smaller than one XRGB row"));
        }
        let minimum_size = u64::from(self.pitch)
            .checked_mul(u64::from(self.physical_height))
            .ok_or_else(|| invalid_data("swapchain size overflow"))?;
        if self.buffer_size < minimum_size {
            return Err(invalid_data("swapchain buffer is smaller than its layout"));
        }
        Ok(self)
    }

    /// How this swapchain's physical scanout layout relates to the
    /// compositor's logical landscape coordinates.
    ///
    /// The presenter declares its physical size in the swapchain header, so
    /// the transform is derived rather than assumed. A panel that scans out
    /// portrait (Apple silicon `adp`) and one that is already landscape
    /// (Intel T2 `appletbdrm`) therefore share one contract, and a buffer
    /// whose physical size matches neither orientation is rejected.
    pub fn transform(self) -> Option<OutputTransform> {
        if self.physical_width == self.logical_width && self.physical_height == self.logical_height
        {
            Some(OutputTransform::Identity)
        } else if self.physical_width == self.logical_height
            && self.physical_height == self.logical_width
        {
            Some(OutputTransform::QuarterTurn)
        } else {
            None
        }
    }

    fn encode(self) -> io::Result<[u8; SWAPCHAIN_HEADER_SIZE]> {
        let info = self.validate()?;
        let mut header = [0_u8; SWAPCHAIN_HEADER_SIZE];
        header[0..8].copy_from_slice(&SWAPCHAIN_MAGIC);
        put_u16(&mut header, 8, SWAPCHAIN_VERSION);
        put_u16(&mut header, 10, info.buffer_count);
        put_u32(&mut header, 12, info.logical_width);
        put_u32(&mut header, 16, info.logical_height);
        put_u32(&mut header, 20, info.physical_width);
        put_u32(&mut header, 24, info.physical_height);
        put_u32(&mut header, 28, info.format);
        put_u32(&mut header, 32, info.pitch);
        put_u64(&mut header, 40, info.buffer_size);
        Ok(header)
    }

    fn decode(header: &[u8; SWAPCHAIN_HEADER_SIZE]) -> io::Result<Self> {
        if header[0..8] != SWAPCHAIN_MAGIC {
            return Err(invalid_data("invalid hardware swapchain magic"));
        }
        if get_u16(header, 8) != SWAPCHAIN_VERSION {
            return Err(invalid_data(
                "unsupported hardware swapchain protocol version",
            ));
        }
        Self {
            buffer_count: get_u16(header, 10),
            logical_width: get_u32(header, 12),
            logical_height: get_u32(header, 16),
            physical_width: get_u32(header, 20),
            physical_height: get_u32(header, 24),
            format: get_u32(header, 28),
            pitch: get_u32(header, 32),
            buffer_size: get_u64(header, 40),
        }
        .validate()
    }
}

pub fn send_hardware_swapchain(
    stream: &UnixStream,
    info: HardwareSwapchain,
    buffers: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let header = info.encode()?;
    if buffers.len() != usize::from(info.buffer_count) {
        return Err(invalid_data(
            "descriptor count does not match swapchain header",
        ));
    }

    let mut iov = libc::iovec {
        iov_base: header.as_ptr().cast_mut().cast(),
        iov_len: header.len(),
    };
    let rights_bytes = (buffers.len() * mem::size_of::<RawFd>()) as u32;
    // A usize array guarantees the alignment required by cmsghdr.
    let mut control = [0_usize; 8];
    let control_len = unsafe { libc::CMSG_SPACE(rights_bytes) as usize };
    if control_len > mem::size_of_val(&control) {
        return Err(invalid_data("descriptor control message is too large"));
    }

    // SAFETY: every pointer in the message refers to live storage for this call.
    let sent = unsafe {
        let mut message: libc::msghdr = mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control_len;

        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(invalid_data("could not construct descriptor message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(rights_bytes) as usize;
        let destination = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        for (index, fd) in buffers.iter().enumerate() {
            ptr::write(destination.add(index), fd.as_raw_fd());
        }
        libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != header.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short ADP swapchain descriptor message",
        ));
    }
    Ok(())
}

pub fn receive_hardware_swapchain(
    stream: &UnixStream,
) -> io::Result<(HardwareSwapchain, Vec<OwnedFd>)> {
    let mut header = [0_u8; SWAPCHAIN_HEADER_SIZE];
    let mut iov = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: header.len(),
    };
    // Large enough for the protocol maximum, with cmsghdr alignment.
    let mut control = [0_usize; 8];

    // SAFETY: every pointer in the message refers to live writable storage.
    let (received, flags, descriptors) = unsafe {
        let mut message: libc::msghdr = mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = mem::size_of_val(&control);
        let received = libc::recvmsg(
            stream.as_raw_fd(),
            &mut message,
            libc::MSG_WAITALL | libc::MSG_CMSG_CLOEXEC,
        );
        if received < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut descriptors = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                return Err(invalid_data("unexpected swapchain ancillary message"));
            }
            let header_len = libc::CMSG_LEN(0) as usize;
            if (*cmsg).cmsg_len < header_len {
                return Err(invalid_data("malformed descriptor control message"));
            }
            let payload_len = (*cmsg).cmsg_len - header_len;
            if !payload_len.is_multiple_of(mem::size_of::<RawFd>()) {
                return Err(invalid_data("malformed descriptor payload"));
            }
            let count = payload_len / mem::size_of::<RawFd>();
            let source = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for index in 0..count {
                let raw_fd = ptr::read(source.add(index));
                // Ownership of SCM_RIGHTS descriptors transfers to this process.
                descriptors.push(OwnedFd::from_raw_fd(raw_fd));
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
        (received, message.msg_flags, descriptors)
    };

    if flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
        return Err(invalid_data("truncated ADP swapchain message"));
    }
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "presenter closed before sending a swapchain",
        ));
    }
    if received as usize != header.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short ADP swapchain header",
        ));
    }

    let info = HardwareSwapchain::decode(&header)?;
    if descriptors.len() != usize::from(info.buffer_count) {
        return Err(invalid_data(
            "received descriptor count does not match header",
        ));
    }
    Ok((info, descriptors))
}

pub fn send_session_message(stream: &mut UnixStream, event: SessionMessage) -> io::Result<()> {
    let fields = match event {
        SessionMessage::FrameReady { index, sequence } => {
            WireFields::new(1, u32::from(index), sequence, 0, 0)
        }
        SessionMessage::Key { key, phase } => WireFields::new(
            8,
            u32::from(key as u16),
            match phase {
                KeyPhase::Released => 0,
                KeyPhase::Pressed => 1,
                KeyPhase::Repeat => 2,
            },
            0,
            0,
        ),
    };
    send_fields(stream, fields)
}

pub fn send_hardware_message(stream: &mut UnixStream, event: HardwareMessage) -> io::Result<()> {
    let fields = match event {
        HardwareMessage::BufferReleased { index, sequence } => {
            WireFields::new(2, u32::from(index), sequence, 0, 0)
        }
        HardwareMessage::Touch(touch) => WireFields::new(
            match touch.phase {
                TouchPhase::Down => 3,
                TouchPhase::Motion => 4,
                TouchPhase::Up => 5,
                TouchPhase::Cancel => 6,
            },
            touch.contact_id,
            u64::from(touch.time_ms),
            touch.x_millipixels,
            touch.y_millipixels,
        ),
        HardwareMessage::FnChanged { pressed } => WireFields::new(7, u32::from(pressed), 0, 0, 0),
    };
    send_fields(stream, fields)
}

#[derive(Clone, Copy)]
struct WireFields {
    kind: u16,
    value: u32,
    payload: u64,
    x: i32,
    y: i32,
}

impl WireFields {
    const fn new(kind: u16, value: u32, payload: u64, x: i32, y: i32) -> Self {
        Self {
            kind,
            value,
            payload,
            x,
            y,
        }
    }
}

fn send_fields(stream: &mut UnixStream, fields: WireFields) -> io::Result<()> {
    let mut message = [0_u8; MESSAGE_SIZE];
    message[0..8].copy_from_slice(&MESSAGE_MAGIC);
    put_u16(&mut message, 8, MESSAGE_VERSION);
    let WireFields {
        kind,
        value,
        payload,
        x,
        y,
    } = fields;
    put_u16(&mut message, 10, kind);
    put_u32(&mut message, 12, value);
    put_u64(&mut message, 16, payload);
    put_i32(&mut message, 24, x);
    put_i32(&mut message, 28, y);
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    // SAFETY: the iovec points at the live event array for this call.
    let sent = unsafe {
        let mut header: libc::msghdr = mem::zeroed();
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != message.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short ADP swapchain event",
        ));
    }
    Ok(())
}

fn receive_fields(stream: &mut UnixStream) -> io::Result<WireFields> {
    let mut message = [0_u8; MESSAGE_SIZE];
    let mut iov = libc::iovec {
        iov_base: message.as_mut_ptr().cast(),
        iov_len: message.len(),
    };
    // SAFETY: the iovec points at the live writable event array for this call.
    let received = unsafe {
        let mut header: libc::msghdr = mem::zeroed();
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_WAITALL)
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "presenter closed before sending an event",
        ));
    }
    if received as usize != message.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short ADP swapchain event",
        ));
    }
    if message[0..8] != MESSAGE_MAGIC {
        return Err(invalid_data("invalid hardware message magic"));
    }
    if get_u16(&message, 8) != MESSAGE_VERSION {
        return Err(invalid_data("unsupported hardware message version"));
    }
    Ok(WireFields::new(
        get_u16(&message, 10),
        get_u32(&message, 12),
        get_u64(&message, 16),
        get_i32(&message, 24),
        get_i32(&message, 28),
    ))
}

pub fn receive_session_message(stream: &mut UnixStream) -> io::Result<SessionMessage> {
    let fields = receive_fields(stream)?;
    if fields.x != 0 || fields.y != 0 {
        return Err(invalid_data("session message has nonzero reserved fields"));
    }
    match fields.kind {
        1 if fields.value <= u32::from(u16::MAX) => Ok(SessionMessage::FrameReady {
            index: fields.value as u16,
            sequence: fields.payload,
        }),
        1 => Err(invalid_data("hardware buffer index is out of range")),
        8 if fields.payload <= 2 => Ok(SessionMessage::Key {
            key: SystemKey::decode(fields.value)?,
            phase: match fields.payload {
                0 => KeyPhase::Released,
                1 => KeyPhase::Pressed,
                2 => KeyPhase::Repeat,
                _ => unreachable!(),
            },
        }),
        8 => Err(invalid_data("unknown system key phase")),
        _ => Err(invalid_data("message is not valid from a user session")),
    }
}

pub fn receive_hardware_message(stream: &mut UnixStream) -> io::Result<HardwareMessage> {
    let fields = receive_fields(stream)?;
    match fields.kind {
        2 if fields.value <= u32::from(u16::MAX) && fields.x == 0 && fields.y == 0 => {
            Ok(HardwareMessage::BufferReleased {
                index: fields.value as u16,
                sequence: fields.payload,
            })
        }
        2 => Err(invalid_data("invalid hardware buffer release")),
        kind @ 3..=6 if fields.payload > u64::from(u32::MAX) => {
            let _ = kind;
            Err(invalid_data("Touch Bar event time is out of range"))
        }
        kind @ 3..=6 => Ok(HardwareMessage::Touch(TouchEvent {
            phase: match kind {
                3 => TouchPhase::Down,
                4 => TouchPhase::Motion,
                5 => TouchPhase::Up,
                6 => TouchPhase::Cancel,
                _ => unreachable!(),
            },
            contact_id: fields.value,
            time_ms: fields.payload as u32,
            x_millipixels: fields.x,
            y_millipixels: fields.y,
        })),
        7 if fields.value <= 1 && fields.payload == 0 && fields.x == 0 && fields.y == 0 => {
            Ok(HardwareMessage::FnChanged {
                pressed: fields.value == 1,
            })
        }
        7 => Err(invalid_data("invalid Fn state message")),
        _ => Err(invalid_data(
            "message is not valid from the hardware daemon",
        )),
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn put_u16(target: &mut [u8], offset: usize, value: u16) {
    target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(target: &mut [u8], offset: usize, value: u32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(target: &mut [u8], offset: usize, value: u64) {
    target[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(target: &mut [u8], offset: usize, value: i32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(source: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        source[offset..offset + 2]
            .try_into()
            .expect("fixed header field"),
    )
}

fn get_u32(source: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        source[offset..offset + 4]
            .try_into()
            .expect("fixed header field"),
    )
}

fn get_u64(source: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        source[offset..offset + 8]
            .try_into()
            .expect("fixed header field"),
    )
}

fn get_i32(source: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(
        source[offset..offset + 4]
            .try_into()
            .expect("fixed header field"),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs::File, os::fd::AsFd};

    use super::*;

    #[test]
    fn portrait_scanout_is_a_quarter_turn_of_logical_coordinates() {
        let adp = info();
        assert_eq!(adp.transform(), Some(OutputTransform::QuarterTurn));
    }

    #[test]
    fn landscape_scanout_needs_no_rotation() {
        let landscape = HardwareSwapchain {
            physical_width: 2008,
            physical_height: 60,
            pitch: 2008 * 4,
            buffer_size: 2008 * 4 * 60,
            ..info()
        };
        assert_eq!(landscape.transform(), Some(OutputTransform::Identity));
        landscape.validate().unwrap();
    }

    #[test]
    fn scanout_matching_neither_orientation_has_no_transform() {
        let skewed = HardwareSwapchain {
            physical_width: 61,
            physical_height: 2008,
            ..info()
        };
        assert_eq!(skewed.transform(), None);
    }

    fn info() -> HardwareSwapchain {
        HardwareSwapchain {
            logical_width: 2008,
            logical_height: 60,
            physical_width: 60,
            physical_height: 2008,
            format: u32::from_le_bytes(*b"XR24"),
            pitch: 256,
            buffer_size: 256 * 2008,
            buffer_count: 2,
        }
    }

    #[test]
    fn transfers_valid_close_on_exec_descriptors() {
        let (sender, receiver) = UnixStream::pair().expect("socket pair");
        let first = File::open("/dev/null").expect("open first fd");
        let second = File::open("/dev/zero").expect("open second fd");
        send_hardware_swapchain(&sender, info(), &[first.as_fd(), second.as_fd()]).expect("send");

        let (received_info, descriptors) = receive_hardware_swapchain(&receiver).expect("receive");
        assert_eq!(received_info, info());
        assert_eq!(descriptors.len(), 2);
        for descriptor in descriptors {
            // SAFETY: F_GETFD only queries the valid received descriptor.
            let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }

        // The managed test sandbox intentionally seals a socket after it has
        // carried SCM_RIGHTS. Exercise event framing on an independent pair;
        // the hardware acceptance test covers both on one production stream.
        let (mut event_sender, mut event_receiver) = UnixStream::pair().expect("event socket pair");
        let ready = SessionMessage::FrameReady {
            index: 1,
            sequence: 42,
        };
        send_session_message(&mut event_sender, ready).expect("send ready event");
        assert_eq!(
            receive_session_message(&mut event_receiver).expect("receive ready"),
            ready
        );

        let released = HardwareMessage::BufferReleased {
            index: 1,
            sequence: 42,
        };
        send_hardware_message(&mut event_receiver, released).expect("send release event");
        assert_eq!(
            receive_hardware_message(&mut event_sender).expect("receive release"),
            released
        );

        let touch = HardwareMessage::Touch(TouchEvent {
            phase: TouchPhase::Motion,
            contact_id: 77,
            time_ms: 1234,
            x_millipixels: -12_500,
            y_millipixels: 59_750,
        });
        send_hardware_message(&mut event_sender, touch).expect("send touch event");
        assert_eq!(
            receive_hardware_message(&mut event_receiver).expect("receive touch"),
            touch
        );
    }

    #[test]
    fn system_key_authority_is_typed_bounded_and_directional() {
        let keys = [
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
        ];
        for key in keys {
            for phase in [KeyPhase::Pressed, KeyPhase::Repeat, KeyPhase::Released] {
                let (mut sender, mut receiver) = UnixStream::pair().expect("socket pair");
                let message = SessionMessage::Key { key, phase };
                send_session_message(&mut sender, message).expect("send system key");
                assert_eq!(
                    receive_session_message(&mut receiver).expect("receive system key"),
                    message
                );
            }
        }

        let (mut sender, mut receiver) = UnixStream::pair().expect("socket pair");
        send_hardware_message(&mut sender, HardwareMessage::FnChanged { pressed: true })
            .expect("send Fn state");
        assert!(receive_session_message(&mut receiver).is_err());
    }

    #[test]
    fn rejects_descriptor_count_mismatch() {
        let (sender, _receiver) = UnixStream::pair().expect("socket pair");
        let file = File::open("/dev/null").expect("open fd");
        let error =
            send_hardware_swapchain(&sender, info(), &[file.as_fd()]).expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_undersized_buffers() {
        let mut invalid = info();
        invalid.buffer_size -= 1;
        let error = invalid.validate().expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn hardware_wire_uses_the_fresh_touchbar_magics() {
        assert_eq!(&SWAPCHAIN_MAGIC, b"TBARHWD1");
        assert_eq!(&MESSAGE_MAGIC, b"TBARMSG1");
    }
}
