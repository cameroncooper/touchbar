//! Private, bounded supervisor-to-component-host transport.
//!
//! Identity and grants deliberately do not appear in [`HostMessage`]. They are
//! attached to the supervisor-side connection record and cannot be asserted by
//! a compromised component host.

use std::{
    fmt, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};

pub const BROKER_PROTOCOL_VERSION: u16 = 1;
pub const MAX_BROKER_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 60 * 1024;
pub const MAX_CAPABILITY_ID_BYTES: usize = 128;
pub const MAX_OPERATION_ID_BYTES: usize = 128;
pub const DEFAULT_ACTIVATION_LIFETIME: Duration = Duration::from_secs(2);

/// Absolute Linux monotonic time shared by the trusted host and supervisor.
pub fn monotonic_micros() -> io::Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is writable storage and CLOCK_MONOTONIC has no other
    // pointer or lifetime requirements.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds =
        u64::try_from(value.tv_sec).map_err(|_| io::Error::other("negative monotonic clock"))?;
    let nanos = u64::try_from(value.tv_nsec)
        .map_err(|_| io::Error::other("negative monotonic nanoseconds"))?;
    Ok(seconds
        .saturating_mul(1_000_000)
        .saturating_add(nanos / 1_000))
}

const MAGIC: [u8; 4] = *b"OTBP";
const HOST_GET_CAPABILITIES: u8 = 1;
const HOST_REQUEST: u8 = 2;
const HOST_CANCEL: u8 = 3;
const HOST_CLOSE: u8 = 4;
const SUPERVISOR_CAPABILITIES: u8 = 101;
const SUPERVISOR_RESPONSE: u8 = 102;
const SUPERVISOR_CAPABILITY_CHANGED: u8 = 103;
const SUPERVISOR_SHUTDOWN: u8 = 104;
const SUPERVISOR_OVERFLOW: u8 = 105;
const SUPERVISOR_RESOURCE_EVENT: u8 = 106;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackPhase {
    Items,
    Render,
    Input,
    HostEvent,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationOrigin {
    Physical,
    TrustedControl,
    Synthetic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationContext {
    pub origin: ActivationOrigin,
    pub surface_instance: u64,
    pub item_id: String,
    pub widget_id: u64,
    pub input_sequence: u64,
    pub deadline_monotonic_micros: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireCapabilityStatus {
    Granted,
    Denied,
    NeedsConsent,
    Unsupported,
    DisclosureOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityState {
    pub capability: String,
    pub required: bool,
    pub status: WireCapabilityStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostMessage {
    GetCapabilities {
        request_id: u64,
    },
    Request {
        request_id: u64,
        phase: CallbackPhase,
        capability: String,
        operation: String,
        payload: Vec<u8>,
        activation: Option<ActivationContext>,
    },
    Cancel {
        request_id: u64,
        target_request_id: u64,
    },
    Close {
        request_id: u64,
        resource_id: u64,
    },
}

impl HostMessage {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::GetCapabilities { request_id }
            | Self::Request { request_id, .. }
            | Self::Cancel { request_id, .. }
            | Self::Close { request_id, .. } => *request_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerErrorCode {
    Unavailable,
    Denied,
    OutOfScope,
    InvalidRequest,
    InvalidPhase,
    ActivationRequired,
    QuotaExceeded,
    RateLimited,
    Timeout,
    Cancelled,
    Unsupported,
    BackendFailed,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerResult {
    Success { payload: Vec<u8> },
    Error(BrokerErrorCode),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorMessage {
    Capabilities {
        request_id: u64,
        generation: u64,
        states: Vec<CapabilityState>,
    },
    Response {
        request_id: u64,
        result: BrokerResult,
    },
    CapabilityChanged {
        generation: u64,
        state: CapabilityState,
    },
    Shutdown {
        generation: u64,
        reason: BrokerErrorCode,
    },
    Overflow {
        generation: u64,
        dropped_events: u64,
    },
    ResourceEvent {
        resource_id: u64,
        sequence: u64,
        result: BrokerResult,
    },
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Disconnected,
    Oversized,
    Malformed(&'static str),
    UnsupportedVersion(u16),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "broker transport I/O failed: {error}"),
            Self::Disconnected => formatter.write_str("broker transport disconnected"),
            Self::Oversized => formatter.write_str("broker packet exceeds the 64 KiB limit"),
            Self::Malformed(message) => write!(formatter, "malformed broker packet: {message}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported broker protocol version {version}")
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected => Self::Disconnected,
            _ => Self::Io(error),
        }
    }
}

#[derive(Debug)]
pub struct Seqpacket {
    fd: OwnedFd,
}

impl Seqpacket {
    pub fn pair() -> Result<(Self, Self), TransportError> {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to two writable ints and socketpair initializes both on success.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                descriptors.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful socketpair returned two uniquely owned descriptors.
        let left = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: successful socketpair returned two uniquely owned descriptors.
        let right = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        Ok((Self { fd: left }, Self { fd: right }))
    }

    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub fn try_from_owned_fd(fd: OwnedFd) -> Result<Self, TransportError> {
        let mut socket_type = 0_i32;
        let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: socket_type and length are writable storage for SO_TYPE.
        let result = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut socket_type as *mut i32).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if socket_type != libc::SOCK_SEQPACKET {
            return Err(TransportError::Malformed(
                "inherited broker descriptor is not SOCK_SEQPACKET",
            ));
        }
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let mut address_length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        // SAFETY: address and address_length are valid writable storage for getsockname.
        let result = unsafe {
            libc::getsockname(
                fd.as_raw_fd(),
                (&mut address as *mut libc::sockaddr_un).cast(),
                &mut address_length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if address.sun_family != libc::AF_UNIX as libc::sa_family_t {
            return Err(TransportError::Malformed(
                "inherited broker descriptor is not AF_UNIX",
            ));
        }
        // The supervisor clears CLOEXEC only long enough for exec. Restore it
        // before any component or runtime worker can spawn another process.
        // SAFETY: F_SETFD operates on this live descriptor.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }

    pub fn send_host(&self, message: &HostMessage) -> Result<(), TransportError> {
        self.send_packet(&encode_host(message)?)
    }

    pub fn recv_host(&self) -> Result<HostMessage, TransportError> {
        decode_host(&self.recv_packet()?)
    }

    pub fn send_supervisor(&self, message: &SupervisorMessage) -> Result<(), TransportError> {
        self.send_packet(&encode_supervisor(message)?)
    }

    pub fn recv_supervisor(&self) -> Result<SupervisorMessage, TransportError> {
        decode_supervisor(&self.recv_packet()?)
    }

    /// Receives one complete supervisor message without waiting for input.
    ///
    /// `SOCK_SEQPACKET` preserves message boundaries, so an available packet is
    /// still decoded atomically. `None` means that no packet was queued at the
    /// instant of the call; it does not mean that the peer disconnected.
    pub fn try_recv_supervisor(&self) -> Result<Option<SupervisorMessage>, TransportError> {
        self.recv_packet_with_flags(libc::MSG_DONTWAIT)?
            .map(|packet| decode_supervisor(&packet))
            .transpose()
    }

    /// Sends one application-defined packet over this authenticated local channel.
    ///
    /// This is intended for narrowly typed supervisor-owned side channels. Callers
    /// remain responsible for validating the payload with their protocol schema.
    pub fn send_payload(&self, payload: &[u8]) -> Result<(), TransportError> {
        self.send_packet(payload)
    }

    /// Receives one application-defined packet without waiting for input.
    pub fn try_recv_payload(&self) -> Result<Option<Vec<u8>>, TransportError> {
        self.recv_packet_with_flags(libc::MSG_DONTWAIT)
    }

    pub fn peer_credentials(&self) -> Result<PeerCredentials, TransportError> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: credentials and length are valid writable storage for SO_PEERCRED.
        let result = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if length as usize != std::mem::size_of::<libc::ucred>() {
            return Err(TransportError::Malformed("invalid peer credentials"));
        }
        Ok(PeerCredentials {
            pid: credentials.pid as u32,
            uid: credentials.uid,
            gid: credentials.gid,
        })
    }

    fn send_packet(&self, packet: &[u8]) -> Result<(), TransportError> {
        if packet.len() > MAX_BROKER_PACKET_BYTES {
            return Err(TransportError::Oversized);
        }
        // SAFETY: packet is readable for its length and the descriptor is a live socket.
        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if sent as usize != packet.len() {
            return Err(TransportError::Malformed("partial sequenced packet send"));
        }
        Ok(())
    }

    fn recv_packet(&self) -> Result<Vec<u8>, TransportError> {
        self.recv_packet_with_flags(0)?
            .ok_or(TransportError::Malformed(
                "blocking receive returned no packet",
            ))
    }

    fn recv_packet_with_flags(&self, flags: i32) -> Result<Option<Vec<u8>>, TransportError> {
        let mut packet = vec![0_u8; MAX_BROKER_PACKET_BYTES + 1];
        // SAFETY: packet is writable for its capacity and the descriptor is a live socket.
        let received = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                flags,
            )
        };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        if received == 0 {
            return Err(TransportError::Disconnected);
        }
        if received as usize > MAX_BROKER_PACKET_BYTES {
            return Err(TransportError::Oversized);
        }
        packet.truncate(received as usize);
        Ok(Some(packet))
    }
}

impl AsRawFd for Seqpacket {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.as_raw_fd()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

fn encode_host(message: &HostMessage) -> Result<Vec<u8>, TransportError> {
    let mut encoder = Encoder::new(match message {
        HostMessage::GetCapabilities { .. } => HOST_GET_CAPABILITIES,
        HostMessage::Request { .. } => HOST_REQUEST,
        HostMessage::Cancel { .. } => HOST_CANCEL,
        HostMessage::Close { .. } => HOST_CLOSE,
    });
    encoder.u64(message.request_id());
    match message {
        HostMessage::GetCapabilities { .. } => {}
        HostMessage::Request {
            phase,
            capability,
            operation,
            payload,
            activation,
            ..
        } => {
            if payload.len() > MAX_INLINE_PAYLOAD_BYTES {
                return Err(TransportError::Oversized);
            }
            encoder.u8(phase_code(*phase));
            encoder.string(capability, MAX_CAPABILITY_ID_BYTES)?;
            encoder.string(operation, MAX_OPERATION_ID_BYTES)?;
            encoder.bytes(payload)?;
            encoder.u8(u8::from(activation.is_some()));
            if let Some(activation) = activation {
                encoder.u8(origin_code(activation.origin));
                encoder.u64(activation.surface_instance);
                encoder.string(&activation.item_id, 256)?;
                encoder.u64(activation.widget_id);
                encoder.u64(activation.input_sequence);
                encoder.u64(activation.deadline_monotonic_micros);
            }
        }
        HostMessage::Cancel {
            target_request_id, ..
        } => encoder.u64(*target_request_id),
        HostMessage::Close { resource_id, .. } => encoder.u64(*resource_id),
    }
    encoder.finish()
}

fn decode_host(packet: &[u8]) -> Result<HostMessage, TransportError> {
    let (kind, mut decoder) = Decoder::new(packet)?;
    let request_id = decoder.u64()?;
    let message = match kind {
        HOST_GET_CAPABILITIES => HostMessage::GetCapabilities { request_id },
        HOST_REQUEST => {
            let phase = decode_phase(decoder.u8()?)?;
            let capability = decoder.string(MAX_CAPABILITY_ID_BYTES)?;
            let operation = decoder.string(MAX_OPERATION_ID_BYTES)?;
            let payload = decoder.bytes(MAX_INLINE_PAYLOAD_BYTES)?.to_vec();
            let activation = match decoder.u8()? {
                0 => None,
                1 => Some(ActivationContext {
                    origin: decode_origin(decoder.u8()?)?,
                    surface_instance: decoder.u64()?,
                    item_id: decoder.string(256)?,
                    widget_id: decoder.u64()?,
                    input_sequence: decoder.u64()?,
                    deadline_monotonic_micros: decoder.u64()?,
                }),
                _ => return Err(TransportError::Malformed("invalid activation marker")),
            };
            HostMessage::Request {
                request_id,
                phase,
                capability,
                operation,
                payload,
                activation,
            }
        }
        HOST_CANCEL => HostMessage::Cancel {
            request_id,
            target_request_id: decoder.u64()?,
        },
        HOST_CLOSE => HostMessage::Close {
            request_id,
            resource_id: decoder.u64()?,
        },
        _ => return Err(TransportError::Malformed("unknown host message kind")),
    };
    decoder.finish()?;
    Ok(message)
}

fn encode_supervisor(message: &SupervisorMessage) -> Result<Vec<u8>, TransportError> {
    let mut encoder = Encoder::new(match message {
        SupervisorMessage::Capabilities { .. } => SUPERVISOR_CAPABILITIES,
        SupervisorMessage::Response { .. } => SUPERVISOR_RESPONSE,
        SupervisorMessage::CapabilityChanged { .. } => SUPERVISOR_CAPABILITY_CHANGED,
        SupervisorMessage::Shutdown { .. } => SUPERVISOR_SHUTDOWN,
        SupervisorMessage::Overflow { .. } => SUPERVISOR_OVERFLOW,
        SupervisorMessage::ResourceEvent { .. } => SUPERVISOR_RESOURCE_EVENT,
    });
    match message {
        SupervisorMessage::Capabilities {
            request_id,
            generation,
            states,
        } => {
            encoder.u64(*request_id);
            encoder.u64(*generation);
            encoder.states(states)?;
        }
        SupervisorMessage::Response { request_id, result } => {
            encoder.u64(*request_id);
            match result {
                BrokerResult::Success { payload } => {
                    if payload.len() > MAX_INLINE_PAYLOAD_BYTES {
                        return Err(TransportError::Oversized);
                    }
                    encoder.u8(0);
                    encoder.bytes(payload)?;
                }
                BrokerResult::Error(error) => {
                    encoder.u8(1);
                    encoder.u8(error_code(*error));
                }
            }
        }
        SupervisorMessage::CapabilityChanged { generation, state } => {
            encoder.u64(*generation);
            encoder.state(state)?;
        }
        SupervisorMessage::Shutdown { generation, reason } => {
            encoder.u64(*generation);
            encoder.u8(error_code(*reason));
        }
        SupervisorMessage::Overflow {
            generation,
            dropped_events,
        } => {
            encoder.u64(*generation);
            encoder.u64(*dropped_events);
        }
        SupervisorMessage::ResourceEvent {
            resource_id,
            sequence,
            result,
        } => {
            encoder.u64(*resource_id);
            encoder.u64(*sequence);
            encode_result(&mut encoder, result)?;
        }
    }
    encoder.finish()
}

fn decode_supervisor(packet: &[u8]) -> Result<SupervisorMessage, TransportError> {
    let (kind, mut decoder) = Decoder::new(packet)?;
    let message = match kind {
        SUPERVISOR_CAPABILITIES => SupervisorMessage::Capabilities {
            request_id: decoder.u64()?,
            generation: decoder.u64()?,
            states: decoder.states()?,
        },
        SUPERVISOR_RESPONSE => {
            let request_id = decoder.u64()?;
            let result = match decoder.u8()? {
                0 => BrokerResult::Success {
                    payload: decoder.bytes(MAX_INLINE_PAYLOAD_BYTES)?.to_vec(),
                },
                1 => BrokerResult::Error(decode_error(decoder.u8()?)?),
                _ => return Err(TransportError::Malformed("invalid response result")),
            };
            SupervisorMessage::Response { request_id, result }
        }
        SUPERVISOR_CAPABILITY_CHANGED => SupervisorMessage::CapabilityChanged {
            generation: decoder.u64()?,
            state: decoder.state()?,
        },
        SUPERVISOR_SHUTDOWN => SupervisorMessage::Shutdown {
            generation: decoder.u64()?,
            reason: decode_error(decoder.u8()?)?,
        },
        SUPERVISOR_OVERFLOW => SupervisorMessage::Overflow {
            generation: decoder.u64()?,
            dropped_events: decoder.u64()?,
        },
        SUPERVISOR_RESOURCE_EVENT => SupervisorMessage::ResourceEvent {
            resource_id: decoder.u64()?,
            sequence: decoder.u64()?,
            result: decode_result(&mut decoder)?,
        },
        _ => return Err(TransportError::Malformed("unknown supervisor message kind")),
    };
    decoder.finish()?;
    Ok(message)
}

fn encode_result(encoder: &mut Encoder, result: &BrokerResult) -> Result<(), TransportError> {
    match result {
        BrokerResult::Success { payload } => {
            if payload.len() > MAX_INLINE_PAYLOAD_BYTES {
                return Err(TransportError::Oversized);
            }
            encoder.u8(0);
            encoder.bytes(payload)
        }
        BrokerResult::Error(error) => {
            encoder.u8(1);
            encoder.u8(error_code(*error));
            Ok(())
        }
    }
}

fn decode_result(decoder: &mut Decoder<'_>) -> Result<BrokerResult, TransportError> {
    match decoder.u8()? {
        0 => Ok(BrokerResult::Success {
            payload: decoder.bytes(MAX_INLINE_PAYLOAD_BYTES)?.to_vec(),
        }),
        1 => Ok(BrokerResult::Error(decode_error(decoder.u8()?)?)),
        _ => Err(TransportError::Malformed("invalid broker result")),
    }
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(kind: u8) -> Self {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&BROKER_PROTOCOL_VERSION.to_le_bytes());
        bytes.push(kind);
        bytes.push(0);
        Self { bytes }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str, maximum: usize) -> Result<(), TransportError> {
        if value.is_empty() || value.len() > maximum || value.len() > u16::MAX as usize {
            return Err(TransportError::Malformed(
                "string length is outside its limit",
            ));
        }
        self.u16(value.len() as u16);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), TransportError> {
        let length = u32::try_from(value.len()).map_err(|_| TransportError::Oversized)?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn state(&mut self, state: &CapabilityState) -> Result<(), TransportError> {
        self.string(&state.capability, MAX_CAPABILITY_ID_BYTES)?;
        self.u8(u8::from(state.required));
        self.u8(status_code(state.status));
        Ok(())
    }

    fn states(&mut self, states: &[CapabilityState]) -> Result<(), TransportError> {
        if states.len() > 64 {
            return Err(TransportError::Malformed("too many capability states"));
        }
        self.u16(states.len() as u16);
        for state in states {
            self.state(state)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, TransportError> {
        if self.bytes.len() > MAX_BROKER_PACKET_BYTES {
            Err(TransportError::Oversized)
        } else {
            Ok(self.bytes)
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Result<(u8, Self), TransportError> {
        if bytes.len() < 8 || bytes[..4] != MAGIC {
            return Err(TransportError::Malformed("bad header magic"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != BROKER_PROTOCOL_VERSION {
            return Err(TransportError::UnsupportedVersion(version));
        }
        if bytes[7] != 0 {
            return Err(TransportError::Malformed("nonzero reserved header byte"));
        }
        Ok((bytes[6], Self { bytes, offset: 8 }))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], TransportError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(TransportError::Malformed("length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(TransportError::Malformed("truncated packet"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, TransportError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, TransportError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, TransportError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("four-byte slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, TransportError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("eight-byte slice"),
        ))
    }

    fn string(&mut self, maximum: usize) -> Result<String, TransportError> {
        let length = self.u16()? as usize;
        if length == 0 || length > maximum {
            return Err(TransportError::Malformed(
                "string length is outside its limit",
            ));
        }
        let value = std::str::from_utf8(self.take(length)?)
            .map_err(|_| TransportError::Malformed("string is not UTF-8"))?;
        Ok(value.into())
    }

    fn bytes(&mut self, maximum: usize) -> Result<&'a [u8], TransportError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(TransportError::Oversized);
        }
        self.take(length)
    }

    fn state(&mut self) -> Result<CapabilityState, TransportError> {
        let capability = self.string(MAX_CAPABILITY_ID_BYTES)?;
        let required = match self.u8()? {
            0 => false,
            1 => true,
            _ => return Err(TransportError::Malformed("invalid required marker")),
        };
        Ok(CapabilityState {
            capability,
            required,
            status: decode_status(self.u8()?)?,
        })
    }

    fn states(&mut self) -> Result<Vec<CapabilityState>, TransportError> {
        let count = self.u16()? as usize;
        if count > 64 {
            return Err(TransportError::Malformed("too many capability states"));
        }
        (0..count).map(|_| self.state()).collect()
    }

    fn finish(self) -> Result<(), TransportError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(TransportError::Malformed("trailing packet bytes"))
        }
    }
}

fn phase_code(value: CallbackPhase) -> u8 {
    match value {
        CallbackPhase::Items => 1,
        CallbackPhase::Render => 2,
        CallbackPhase::Input => 3,
        CallbackPhase::HostEvent => 4,
        CallbackPhase::Lifecycle => 5,
    }
}

fn decode_phase(value: u8) -> Result<CallbackPhase, TransportError> {
    match value {
        1 => Ok(CallbackPhase::Items),
        2 => Ok(CallbackPhase::Render),
        3 => Ok(CallbackPhase::Input),
        4 => Ok(CallbackPhase::HostEvent),
        5 => Ok(CallbackPhase::Lifecycle),
        _ => Err(TransportError::Malformed("invalid callback phase")),
    }
}

fn origin_code(value: ActivationOrigin) -> u8 {
    match value {
        ActivationOrigin::Physical => 1,
        ActivationOrigin::TrustedControl => 2,
        ActivationOrigin::Synthetic => 3,
    }
}

fn decode_origin(value: u8) -> Result<ActivationOrigin, TransportError> {
    match value {
        1 => Ok(ActivationOrigin::Physical),
        2 => Ok(ActivationOrigin::TrustedControl),
        3 => Ok(ActivationOrigin::Synthetic),
        _ => Err(TransportError::Malformed("invalid activation origin")),
    }
}

fn status_code(value: WireCapabilityStatus) -> u8 {
    match value {
        WireCapabilityStatus::Granted => 1,
        WireCapabilityStatus::Denied => 2,
        WireCapabilityStatus::NeedsConsent => 3,
        WireCapabilityStatus::Unsupported => 4,
        WireCapabilityStatus::DisclosureOnly => 5,
    }
}

fn decode_status(value: u8) -> Result<WireCapabilityStatus, TransportError> {
    match value {
        1 => Ok(WireCapabilityStatus::Granted),
        2 => Ok(WireCapabilityStatus::Denied),
        3 => Ok(WireCapabilityStatus::NeedsConsent),
        4 => Ok(WireCapabilityStatus::Unsupported),
        5 => Ok(WireCapabilityStatus::DisclosureOnly),
        _ => Err(TransportError::Malformed("invalid capability status")),
    }
}

fn error_code(value: BrokerErrorCode) -> u8 {
    match value {
        BrokerErrorCode::Unavailable => 1,
        BrokerErrorCode::Denied => 2,
        BrokerErrorCode::OutOfScope => 3,
        BrokerErrorCode::InvalidRequest => 4,
        BrokerErrorCode::InvalidPhase => 5,
        BrokerErrorCode::ActivationRequired => 6,
        BrokerErrorCode::QuotaExceeded => 7,
        BrokerErrorCode::RateLimited => 8,
        BrokerErrorCode::Timeout => 9,
        BrokerErrorCode::Cancelled => 10,
        BrokerErrorCode::Unsupported => 11,
        BrokerErrorCode::BackendFailed => 12,
        BrokerErrorCode::Internal => 13,
    }
}

fn decode_error(value: u8) -> Result<BrokerErrorCode, TransportError> {
    match value {
        1 => Ok(BrokerErrorCode::Unavailable),
        2 => Ok(BrokerErrorCode::Denied),
        3 => Ok(BrokerErrorCode::OutOfScope),
        4 => Ok(BrokerErrorCode::InvalidRequest),
        5 => Ok(BrokerErrorCode::InvalidPhase),
        6 => Ok(BrokerErrorCode::ActivationRequired),
        7 => Ok(BrokerErrorCode::QuotaExceeded),
        8 => Ok(BrokerErrorCode::RateLimited),
        9 => Ok(BrokerErrorCode::Timeout),
        10 => Ok(BrokerErrorCode::Cancelled),
        11 => Ok(BrokerErrorCode::Unsupported),
        12 => Ok(BrokerErrorCode::BackendFailed),
        13 => Ok(BrokerErrorCode::Internal),
        _ => Err(TransportError::Malformed("invalid broker error code")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_monotonic_clock_advances() {
        let first = monotonic_micros().unwrap();
        std::thread::sleep(Duration::from_millis(1));
        assert!(monotonic_micros().unwrap() > first);
    }

    #[test]
    fn seqpacket_round_trips_without_losing_boundaries() {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let message = HostMessage::Request {
            request_id: 7,
            phase: CallbackPhase::Input,
            capability: "dbus.call.v1".into(),
            operation: "call".into(),
            payload: vec![1, 2, 3],
            activation: Some(ActivationContext {
                origin: ActivationOrigin::Physical,
                surface_instance: 9,
                item_id: "media".into(),
                widget_id: 11,
                input_sequence: 12,
                deadline_monotonic_micros: 13,
            }),
        };
        host.send_host(&message).unwrap();
        assert_eq!(supervisor.recv_host().unwrap(), message);

        let response = SupervisorMessage::Response {
            request_id: 7,
            result: BrokerResult::Error(BrokerErrorCode::Unavailable),
        };
        supervisor.send_supervisor(&response).unwrap();
        assert_eq!(host.recv_supervisor().unwrap(), response);

        let event = SupervisorMessage::ResourceEvent {
            resource_id: 3,
            sequence: 9,
            result: BrokerResult::Success {
                payload: vec![4, 5, 6],
            },
        };
        supervisor.send_supervisor(&event).unwrap();
        assert_eq!(host.recv_supervisor().unwrap(), event);
    }

    #[test]
    fn peer_close_errors_are_normalized_as_disconnects() {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        drop(host);
        assert!(matches!(
            supervisor.send_supervisor(&SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: Vec::new()
                },
            }),
            Err(TransportError::Disconnected)
        ));

        let (host, supervisor) = Seqpacket::pair().unwrap();
        drop(host);
        assert!(matches!(
            supervisor.recv_host(),
            Err(TransportError::Disconnected)
        ));
    }

    #[test]
    fn sockets_are_close_on_exec_and_expose_peer_credentials() {
        let (left, _right) = Seqpacket::pair().unwrap();
        // SAFETY: F_GETFD does not mutate memory and the descriptor is live.
        let flags = unsafe { libc::fcntl(left.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let credentials = left.peer_credentials().unwrap();
        assert_eq!(credentials.pid, std::process::id());
        // SAFETY: geteuid has no preconditions.
        assert_eq!(credentials.uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn inherited_channel_validation_rejects_stream_sockets() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (stream, _peer) = UnixStream::pair().unwrap();
        // SAFETY: into_raw_fd transfers unique ownership into OwnedFd.
        let descriptor = unsafe { OwnedFd::from_raw_fd(stream.into_raw_fd()) };
        assert!(matches!(
            Seqpacket::try_from_owned_fd(descriptor),
            Err(TransportError::Malformed(
                "inherited broker descriptor is not SOCK_SEQPACKET"
            ))
        ));
    }

    #[test]
    fn codec_rejects_trailing_data_bad_versions_and_oversized_payloads() {
        let mut packet = encode_host(&HostMessage::GetCapabilities { request_id: 1 }).unwrap();
        packet.push(0);
        assert!(matches!(
            decode_host(&packet),
            Err(TransportError::Malformed("trailing packet bytes"))
        ));
        packet.truncate(packet.len() - 1);
        packet[4..6].copy_from_slice(&99_u16.to_le_bytes());
        assert!(matches!(
            decode_host(&packet),
            Err(TransportError::UnsupportedVersion(99))
        ));
        let oversized = HostMessage::Request {
            request_id: 2,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "request".into(),
            payload: vec![0; MAX_INLINE_PAYLOAD_BYTES + 1],
            activation: None,
        };
        assert!(matches!(
            encode_host(&oversized),
            Err(TransportError::Oversized)
        ));
    }

    #[test]
    fn deterministic_adversarial_packets_never_panic_or_trust_wire_lengths() {
        let host_seed = encode_host(&HostMessage::Request {
            request_id: 7,
            phase: CallbackPhase::Input,
            capability: "clipboard.read.v1".into(),
            operation: "read".into(),
            payload: vec![1, 2, 3],
            activation: Some(ActivationContext {
                origin: ActivationOrigin::Physical,
                surface_instance: 1,
                item_id: "clipboard".into(),
                widget_id: 2,
                input_sequence: 3,
                deadline_monotonic_micros: 4,
            }),
        })
        .unwrap();
        let supervisor_seed = encode_supervisor(&SupervisorMessage::Response {
            request_id: 7,
            result: BrokerResult::Success {
                payload: vec![4, 5, 6],
            },
        })
        .unwrap();
        let mut random = 0xd1b5_4a32_d192_ed03_u64;
        for index in 0..8192 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let seed = if index % 2 == 0 {
                &host_seed
            } else {
                &supervisor_seed
            };
            let mut packet = match index % 6 {
                0 => seed[..index % (seed.len() + 1)].to_vec(),
                1 => {
                    let mut value = seed.clone();
                    let offset = (random as usize) % value.len();
                    value[offset] ^= (random >> 32) as u8 | 1;
                    value
                }
                2 => {
                    let mut value = seed.clone();
                    value.extend_from_slice(&(random as u32).to_le_bytes());
                    value
                }
                3 if index % 127 == 3 => vec![0xff; MAX_BROKER_PACKET_BYTES + 1],
                _ => {
                    let length = (random as usize) % 512;
                    let mut value = vec![0_u8; length];
                    for byte in &mut value {
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        *byte = random as u8;
                    }
                    value
                }
            };
            if index % 17 == 0 && packet.len() >= 4 {
                packet[..4].copy_from_slice(&seed[..4]);
            }
            let _ = decode_host(&packet);
            let _ = decode_supervisor(&packet);
        }
    }
}
