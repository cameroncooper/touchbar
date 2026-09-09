//! Bounded, typed payloads shared by sandboxed plugins and trusted brokers.
//!
//! The outer WIT broker deliberately carries bytes so adding a capability does
//! not change the component world. Each capability owns exactly one current
//! schema here; raw service protocol messages are never accepted.

use std::{collections::BTreeSet, error::Error, fmt};

const CALL_MAGIC: [u8; 8] = *b"OMDBUSC1";
const REPLY_MAGIC: [u8; 8] = *b"OMDBUSR1";
const SUBSCRIPTION_MAGIC: [u8; 8] = *b"OMDBUSS1";
const OPENED_MAGIC: [u8; 8] = *b"OMDBUSO1";
const SIGNAL_MAGIC: [u8; 8] = *b"OMDBUSE1";
const FILE_READ_MAGIC: [u8; 8] = *b"OMFSRED1";
const FILE_REPLY_MAGIC: [u8; 8] = *b"OMFSRPL1";
const FILE_STREAM_REQUEST_MAGIC: [u8; 8] = *b"OMFSSTQ1";
const FILE_STREAM_OPENED_MAGIC: [u8; 8] = *b"OMFSSOP1";
const FILE_STREAM_EVENT_MAGIC: [u8; 8] = *b"OMFSSTE1";
const DIRECTORY_LIST_MAGIC: [u8; 8] = *b"OMFSLST1";
const DIRECTORY_REPLY_MAGIC: [u8; 8] = *b"OMFSDIR1";
const FILE_WRITE_MAGIC: [u8; 8] = *b"OMFSWRT1";
const FILE_PATH_MAGIC: [u8; 8] = *b"OMFSPTH1";
const FILE_RENAME_MAGIC: [u8; 8] = *b"OMFSREN1";
const FILE_MUTATION_MAGIC: [u8; 8] = *b"OMFSMUT1";
const FILE_WRITE_STREAM_MAGIC: [u8; 8] = *b"OMFSWSQ1";
const FILE_WRITE_STREAM_OPENED_MAGIC: [u8; 8] = *b"OMFSWSO1";
const FILE_WRITE_STREAM_CHUNK_MAGIC: [u8; 8] = *b"OMFSWSC1";
const FILE_WRITE_STREAM_COMMIT_MAGIC: [u8; 8] = *b"OMFSWSM1";
const URI_OPEN_MAGIC: [u8; 8] = *b"OMURIOP1";
const NOTIFICATION_SEND_MAGIC: [u8; 8] = *b"OMNOTIF1";
const NOTIFICATION_REMOVE_MAGIC: [u8; 8] = *b"OMNOTIR1";
const LOCAL_CONNECT_MAGIC: [u8; 8] = *b"OMLOCNQ1";
const LOCAL_OPENED_MAGIC: [u8; 8] = *b"OMLOCNO1";
const LOCAL_SEND_MAGIC: [u8; 8] = *b"OMLOCNS1";
const LOCAL_EVENT_MAGIC: [u8; 8] = *b"OMLOCNE1";
const SECRET_READ_MAGIC: [u8; 8] = *b"OMSECRQ1";
const SECRET_VALUE_MAGIC: [u8; 8] = *b"OMSECRV1";
const CLIPBOARD_READ_MAGIC: [u8; 8] = *b"OMCLPRQ1";
const CLIPBOARD_WRITE_MAGIC: [u8; 8] = *b"OMCLPWQ1";
const CLIPBOARD_VALUE_MAGIC: [u8; 8] = *b"OMCLPVL1";
const CONTEXT_READ_MAGIC: [u8; 8] = *b"OMCTXRQ1";
const CONTEXT_SNAPSHOT_MAGIC: [u8; 8] = *b"OMCTXSN1";
const CONTEXT_OPENED_MAGIC: [u8; 8] = *b"OMCTXOP1";
const APPEARANCE_PUBLISH_MAGIC: [u8; 8] = *b"OMAPPPB1";
const HTTP_REQUEST_MAGIC: [u8; 8] = *b"OMHTTPQ1";
const HTTP_RESPONSE_MAGIC: [u8; 8] = *b"OMHTTPR1";
const HTTP_STREAM_OPENED_MAGIC: [u8; 8] = *b"OMHTTPO1";
const HTTP_STREAM_EVENT_MAGIC: [u8; 8] = *b"OMHTTPE1";
const COMMAND_REQUEST_MAGIC: [u8; 8] = *b"OMCMDQ01";
const COMMAND_OPENED_MAGIC: [u8; 8] = *b"OMCMDO01";
const COMMAND_EVENT_MAGIC: [u8; 8] = *b"OMCMDE01";
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_STRING_BYTES: usize = 4096;
const MAX_ARGUMENTS: usize = 32;
const MAX_PROPERTIES: usize = 64;
pub const MAX_FILE_CHUNK_BYTES: u64 = 48 * 1024;
pub const MAX_FILE_WRITE_BYTES: usize = 48 * 1024;
pub const MAX_FILE_WRITE_STREAM_CHUNK_BYTES: usize = 12 * 1024;
pub const MAX_FILE_STREAM_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_FILE_STREAM_CHUNK_BYTES: usize = 12 * 1024;
pub const MAX_DIRECTORY_ENTRIES: u16 = 256;
/// Leaves room inside the 60 KiB outer broker payload for a maximum-size URL
/// and both bounded metadata fields.
pub const MAX_HTTP_INLINE_BODY_BYTES: u64 = 47 * 1024;
pub const MAX_HTTP_STREAM_CHUNK_BYTES: usize = 12 * 1024;
pub const MAX_COMMAND_VALUES: usize = 32;
pub const MAX_COMMAND_OUTPUT_CHUNK_BYTES: usize = 12 * 1024;
pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 256;
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 4096;
pub const MAX_LOCAL_FRAME_BYTES: usize = 12 * 1024;
pub const MAX_SECRET_BYTES: usize = 48 * 1024;
pub const MAX_SECRET_CONTENT_TYPE_BYTES: usize = 128;
pub const MAX_CLIPBOARD_BYTES: usize = 48 * 1024;
pub const MAX_CLIPBOARD_MIME_BYTES: usize = 128;
pub const MAX_CONTEXT_FACTS: usize = 32;
pub const MAX_CONTEXT_VALUE_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppearanceScheme {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppearanceColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppearancePublish {
    pub provider: String,
    pub scheme: AppearanceScheme,
    pub background: AppearanceColor,
    pub foreground: AppearanceColor,
    pub accent: AppearanceColor,
    pub selection: AppearanceColor,
    pub muted: AppearanceColor,
    pub destructive: AppearanceColor,
}

impl AppearancePublish {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.provider.is_empty() {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(APPEARANCE_PUBLISH_MAGIC);
        encoder.string(&self.provider)?;
        encoder.byte(match self.scheme {
            AppearanceScheme::Dark => 0,
            AppearanceScheme::Light => 1,
        });
        for color in [
            self.background,
            self.foreground,
            self.accent,
            self.selection,
            self.muted,
            self.destructive,
        ] {
            encoder.byte(color.red);
            encoder.byte(color.green);
            encoder.byte(color.blue);
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, APPEARANCE_PUBLISH_MAGIC)?;
        let provider = decoder.string()?;
        if provider.is_empty() {
            return Err(SchemaError::Malformed);
        }
        let scheme = match decoder.byte()? {
            0 => AppearanceScheme::Dark,
            1 => AppearanceScheme::Light,
            _ => return Err(SchemaError::Malformed),
        };
        let mut color = || {
            Ok(AppearanceColor {
                red: decoder.byte()?,
                green: decoder.byte()?,
                blue: decoder.byte()?,
            })
        };
        let value = Self {
            provider,
            scheme,
            background: color()?,
            foreground: color()?,
            accent: color()?,
            selection: color()?,
            muted: color()?,
            destructive: color()?,
        };
        decoder.finish()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReadRequest {
    pub facts: BTreeSet<String>,
}

impl ContextReadRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.facts.is_empty() {
            return Err(SchemaError::Malformed);
        }
        if self.facts.len() > MAX_CONTEXT_FACTS {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(CONTEXT_READ_MAGIC);
        encoder.u16(self.facts.len() as u16);
        for fact in &self.facts {
            encoder.string(fact)?;
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, CONTEXT_READ_MAGIC)?;
        let count = usize::from(decoder.u16()?);
        if count == 0 {
            return Err(SchemaError::Malformed);
        }
        if count > MAX_CONTEXT_FACTS {
            return Err(SchemaError::LimitExceeded);
        }
        let mut facts = BTreeSet::new();
        for _ in 0..count {
            if !facts.insert(decoder.string()?) {
                return Err(SchemaError::Malformed);
            }
        }
        decoder.finish()?;
        Ok(Self { facts })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextFactValue {
    Text(String),
    Boolean(bool),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextFact {
    pub key: String,
    pub value: ContextFactValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub generation: u64,
    pub facts: Vec<ContextFact>,
}

impl ContextSnapshot {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        encode_context_snapshot(CONTEXT_SNAPSHOT_MAGIC, None, self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        decode_context_snapshot(bytes, CONTEXT_SNAPSHOT_MAGIC).map(|(_, snapshot)| snapshot)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSubscriptionOpened {
    pub resource_id: u64,
    pub snapshot: ContextSnapshot,
}

impl ContextSubscriptionOpened {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        encode_context_snapshot(CONTEXT_OPENED_MAGIC, Some(self.resource_id), &self.snapshot)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let (resource_id, snapshot) = decode_context_snapshot(bytes, CONTEXT_OPENED_MAGIC)?;
        let resource_id = resource_id.ok_or(SchemaError::Malformed)?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self {
            resource_id,
            snapshot,
        })
    }
}

fn encode_context_snapshot(
    magic: [u8; 8],
    resource_id: Option<u64>,
    snapshot: &ContextSnapshot,
) -> Result<Vec<u8>, SchemaError> {
    if snapshot.generation == 0 {
        return Err(SchemaError::Malformed);
    }
    if snapshot.facts.len() > MAX_CONTEXT_FACTS {
        return Err(SchemaError::LimitExceeded);
    }
    let mut keys = BTreeSet::new();
    let mut encoder = Encoder::new(magic);
    if let Some(resource_id) = resource_id {
        encoder.u64(resource_id);
    }
    encoder.u64(snapshot.generation);
    encoder.u16(snapshot.facts.len() as u16);
    for fact in &snapshot.facts {
        if !keys.insert(&fact.key) {
            return Err(SchemaError::Malformed);
        }
        encoder.string(&fact.key)?;
        match &fact.value {
            ContextFactValue::Text(value) => {
                if value.len() > MAX_CONTEXT_VALUE_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                encoder.byte(0);
                encoder.string(value)?;
            }
            ContextFactValue::Boolean(value) => {
                encoder.byte(1);
                encoder.byte(u8::from(*value));
            }
        }
    }
    encoder.finish()
}

fn decode_context_snapshot(
    bytes: &[u8],
    magic: [u8; 8],
) -> Result<(Option<u64>, ContextSnapshot), SchemaError> {
    let mut decoder = Decoder::new(bytes, magic)?;
    let resource_id = (magic == CONTEXT_OPENED_MAGIC)
        .then(|| decoder.u64())
        .transpose()?;
    let generation = decoder.u64()?;
    if generation == 0 {
        return Err(SchemaError::Malformed);
    }
    let count = usize::from(decoder.u16()?);
    if count > MAX_CONTEXT_FACTS {
        return Err(SchemaError::LimitExceeded);
    }
    let mut keys = BTreeSet::new();
    let mut facts = Vec::with_capacity(count);
    for _ in 0..count {
        let key = decoder.string()?;
        if !keys.insert(key.clone()) {
            return Err(SchemaError::Malformed);
        }
        let value = match decoder.byte()? {
            0 => {
                let value = decoder.string()?;
                if value.len() > MAX_CONTEXT_VALUE_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                ContextFactValue::Text(value)
            }
            1 => match decoder.byte()? {
                0 => ContextFactValue::Boolean(false),
                1 => ContextFactValue::Boolean(true),
                _ => return Err(SchemaError::Malformed),
            },
            _ => return Err(SchemaError::Malformed),
        };
        facts.push(ContextFact { key, value });
    }
    decoder.finish()?;
    Ok((resource_id, ContextSnapshot { generation, facts }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretReadRequest {
    pub logical_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardReadRequest {
    pub mime_type: String,
}

impl ClipboardReadRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(CLIPBOARD_READ_MAGIC);
        encoder.string(&self.mime_type)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, CLIPBOARD_READ_MAGIC)?;
        let value = Self {
            mime_type: decoder.string()?,
        };
        decoder.finish()?;
        if value.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardWriteRequest {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl ClipboardWriteRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES || self.bytes.len() > MAX_CLIPBOARD_BYTES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(CLIPBOARD_WRITE_MAGIC);
        encoder.string(&self.mime_type)?;
        encoder.blob(&self.bytes)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, CLIPBOARD_WRITE_MAGIC)?;
        let value = Self {
            mime_type: decoder.string()?,
            bytes: decoder.blob(MAX_CLIPBOARD_BYTES)?,
        };
        decoder.finish()?;
        if value.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardValue {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl ClipboardValue {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES || self.bytes.len() > MAX_CLIPBOARD_BYTES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(CLIPBOARD_VALUE_MAGIC);
        encoder.string(&self.mime_type)?;
        encoder.blob(&self.bytes)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, CLIPBOARD_VALUE_MAGIC)?;
        let value = Self {
            mime_type: decoder.string()?,
            bytes: decoder.blob(MAX_CLIPBOARD_BYTES)?,
        };
        decoder.finish()?;
        if value.mime_type.len() > MAX_CLIPBOARD_MIME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

impl SecretReadRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(SECRET_READ_MAGIC);
        encoder.string(&self.logical_name)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, SECRET_READ_MAGIC)?;
        let value = Self {
            logical_name: decoder.string()?,
        };
        decoder.finish()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretValue {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

impl SecretValue {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.bytes.len() > MAX_SECRET_BYTES
            || self.content_type.len() > MAX_SECRET_CONTENT_TYPE_BYTES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(SECRET_VALUE_MAGIC);
        encoder.blob(&self.bytes)?;
        encoder.string(&self.content_type)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, SECRET_VALUE_MAGIC)?;
        let value = Self {
            bytes: decoder.blob(MAX_SECRET_BYTES)?,
            content_type: decoder.string()?,
        };
        decoder.finish()?;
        if value.content_type.len() > MAX_SECRET_CONTENT_TYPE_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalConnect {
    pub endpoint: String,
    pub protocol: String,
}

impl LocalConnect {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(LOCAL_CONNECT_MAGIC);
        encoder.string(&self.endpoint)?;
        encoder.string(&self.protocol)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, LOCAL_CONNECT_MAGIC)?;
        let value = Self {
            endpoint: decoder.string()?,
            protocol: decoder.string()?,
        };
        decoder.finish()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalConnectionOpened {
    pub resource_id: u64,
    pub maximum_frame_bytes: u32,
}

impl LocalConnectionOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0
            || self.maximum_frame_bytes == 0
            || self.maximum_frame_bytes as usize > MAX_LOCAL_FRAME_BYTES
        {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(LOCAL_OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.u32(self.maximum_frame_bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, LOCAL_OPENED_MAGIC)?;
        let value = Self {
            resource_id: decoder.u64()?,
            maximum_frame_bytes: decoder.u32()?,
        };
        decoder.finish()?;
        if value.resource_id == 0
            || value.maximum_frame_bytes == 0
            || value.maximum_frame_bytes as usize > MAX_LOCAL_FRAME_BYTES
        {
            return Err(SchemaError::Malformed);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSendFrame {
    pub resource_id: u64,
    pub bytes: Vec<u8>,
}

impl LocalSendFrame {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 || self.bytes.is_empty() {
            return Err(SchemaError::Malformed);
        }
        if self.bytes.len() > MAX_LOCAL_FRAME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(LOCAL_SEND_MAGIC);
        encoder.u64(self.resource_id);
        encoder.blob(&self.bytes)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, LOCAL_SEND_MAGIC)?;
        let value = Self {
            resource_id: decoder.u64()?,
            bytes: decoder.blob(MAX_LOCAL_FRAME_BYTES)?,
        };
        decoder.finish()?;
        if value.resource_id == 0 || value.bytes.is_empty() {
            return Err(SchemaError::Malformed);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFrameEvent {
    pub bytes: Vec<u8>,
}

impl LocalFrameEvent {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.bytes.is_empty() {
            return Err(SchemaError::Malformed);
        }
        if self.bytes.len() > MAX_LOCAL_FRAME_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(LOCAL_EVENT_MAGIC);
        encoder.blob(&self.bytes)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, LOCAL_EVENT_MAGIC)?;
        let value = Self {
            bytes: decoder.blob(MAX_LOCAL_FRAME_BYTES)?,
        };
        decoder.finish()?;
        if value.bytes.is_empty() {
            return Err(SchemaError::Malformed);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemWriteFile {
    pub mount: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UriOpenRequest {
    pub uri: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationUrgencyValue {
    Low,
    Normal,
    Critical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationSend {
    pub id: String,
    pub category: String,
    pub urgency: NotificationUrgencyValue,
    pub title: String,
    pub body: String,
}

impl NotificationSend {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.title.len() > MAX_NOTIFICATION_TITLE_BYTES
            || self.body.len() > MAX_NOTIFICATION_BODY_BYTES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(NOTIFICATION_SEND_MAGIC);
        encoder.string(&self.id)?;
        encoder.string(&self.category)?;
        encoder.byte(match self.urgency {
            NotificationUrgencyValue::Low => 0,
            NotificationUrgencyValue::Normal => 1,
            NotificationUrgencyValue::Critical => 2,
        });
        encoder.string(&self.title)?;
        encoder.string(&self.body)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, NOTIFICATION_SEND_MAGIC)?;
        let id = decoder.string()?;
        let category = decoder.string()?;
        let urgency = match decoder.byte()? {
            0 => NotificationUrgencyValue::Low,
            1 => NotificationUrgencyValue::Normal,
            2 => NotificationUrgencyValue::Critical,
            _ => return Err(SchemaError::Malformed),
        };
        let title = decoder.string()?;
        let body = decoder.string()?;
        decoder.finish()?;
        if title.len() > MAX_NOTIFICATION_TITLE_BYTES || body.len() > MAX_NOTIFICATION_BODY_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(Self {
            id,
            category,
            urgency,
            title,
            body,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationRemove {
    pub id: String,
}

impl NotificationRemove {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(NOTIFICATION_REMOVE_MAGIC);
        encoder.string(&self.id)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, NOTIFICATION_REMOVE_MAGIC)?;
        let id = decoder.string()?;
        decoder.finish()?;
        Ok(Self { id })
    }
}

impl UriOpenRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.uri.len() > MAX_URI_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(URI_OPEN_MAGIC);
        encoder.string(&self.uri)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, URI_OPEN_MAGIC)?;
        let uri = decoder.string()?;
        decoder.finish()?;
        if uri.len() > MAX_URI_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(Self { uri })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemWriteStream {
    pub mount: String,
    pub path: String,
    pub expected_bytes: u64,
}

impl FilesystemWriteStream {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.expected_bytes > MAX_FILE_STREAM_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_WRITE_STREAM_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.u64(self.expected_bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_WRITE_STREAM_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            path: decoder.string()?,
            expected_bytes: decoder.u64()?,
        };
        decoder.finish()?;
        if value.expected_bytes > MAX_FILE_STREAM_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemWriteStreamOpened {
    pub resource_id: u64,
    pub maximum_chunk_bytes: u32,
}

impl FilesystemWriteStreamOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0
            || self.maximum_chunk_bytes == 0
            || self.maximum_chunk_bytes as usize > MAX_FILE_WRITE_STREAM_CHUNK_BYTES
        {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(FILE_WRITE_STREAM_OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.u32(self.maximum_chunk_bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_WRITE_STREAM_OPENED_MAGIC)?;
        let value = Self {
            resource_id: decoder.u64()?,
            maximum_chunk_bytes: decoder.u32()?,
        };
        decoder.finish()?;
        if value.resource_id == 0
            || value.maximum_chunk_bytes == 0
            || value.maximum_chunk_bytes as usize > MAX_FILE_WRITE_STREAM_CHUNK_BYTES
        {
            return Err(SchemaError::Malformed);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemWriteStreamChunk {
    pub resource_id: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl FilesystemWriteStreamChunk {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_FILE_WRITE_STREAM_CHUNK_BYTES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_WRITE_STREAM_CHUNK_MAGIC);
        encoder.u64(self.resource_id);
        encoder.u64(self.offset);
        encoder.u32(self.bytes.len() as u32);
        encoder.bytes.extend_from_slice(&self.bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_WRITE_STREAM_CHUNK_MAGIC)?;
        let resource_id = decoder.u64()?;
        let offset = decoder.u64()?;
        let length = decoder.u32()? as usize;
        if resource_id == 0 || length == 0 || length > MAX_FILE_WRITE_STREAM_CHUNK_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let bytes = decoder.take(length)?.to_vec();
        decoder.finish()?;
        Ok(Self {
            resource_id,
            offset,
            bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemWriteStreamCommit {
    pub resource_id: u64,
}

impl FilesystemWriteStreamCommit {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(FILE_WRITE_STREAM_COMMIT_MAGIC);
        encoder.u64(self.resource_id);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_WRITE_STREAM_COMMIT_MAGIC)?;
        let resource_id = decoder.u64()?;
        decoder.finish()?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self { resource_id })
    }
}

impl FilesystemWriteFile {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.bytes.len() > MAX_FILE_WRITE_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_WRITE_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.u64(self.bytes.len() as u64);
        encoder.bytes.extend_from_slice(&self.bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_WRITE_MAGIC)?;
        let mount = decoder.string()?;
        let path = decoder.string()?;
        let length = usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
        if length > MAX_FILE_WRITE_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let bytes = decoder.take(length)?.to_vec();
        decoder.finish()?;
        Ok(Self { mount, path, bytes })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemPath {
    pub mount: String,
    pub path: String,
}

impl FilesystemPath {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(FILE_PATH_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_PATH_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            path: decoder.string()?,
        };
        decoder.finish()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemRename {
    pub mount: String,
    pub source: String,
    pub destination: String,
}

impl FilesystemRename {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(FILE_RENAME_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.source)?;
        encoder.string(&self.destination)?;
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_RENAME_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            source: decoder.string()?,
            destination: decoder.string()?,
        };
        decoder.finish()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemMutationResult {
    pub bytes_written: u64,
    pub resulting_size: Option<u64>,
}

impl FilesystemMutationResult {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(FILE_MUTATION_MAGIC);
        encoder.u64(self.bytes_written);
        encoder.byte(u8::from(self.resulting_size.is_some()));
        if let Some(size) = self.resulting_size {
            encoder.u64(size);
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_MUTATION_MAGIC)?;
        let bytes_written = decoder.u64()?;
        let resulting_size = match decoder.byte()? {
            0 => None,
            1 => Some(decoder.u64()?),
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(Self {
            bytes_written,
            resulting_size,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandValue {
    Integer { name: String, value: i64 },
    FixedEnum { name: String, value: String },
    Text { name: String, value: String },
    ApprovedFile { name: String, path: String },
    Url { name: String, value: String },
}

impl CommandValue {
    fn name(&self) -> &str {
        match self {
            Self::Integer { name, .. }
            | Self::FixedEnum { name, .. }
            | Self::Text { name, .. }
            | Self::ApprovedFile { name, .. }
            | Self::Url { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRunRequest {
    pub command_id: String,
    pub values: Vec<CommandValue>,
}

impl CommandRunRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.values.len() > MAX_COMMAND_VALUES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut names = BTreeSet::new();
        let mut encoder = Encoder::new(COMMAND_REQUEST_MAGIC);
        encoder.string(&self.command_id)?;
        encoder.u16(self.values.len() as u16);
        for value in &self.values {
            if !names.insert(value.name()) {
                return Err(SchemaError::Malformed);
            }
            match value {
                CommandValue::Integer { name, value } => {
                    encoder.byte(0);
                    encoder.string(name)?;
                    encoder.bytes.extend_from_slice(&value.to_le_bytes());
                }
                CommandValue::FixedEnum { name, value } => {
                    encoder.byte(1);
                    encoder.string(name)?;
                    encoder.string(value)?;
                }
                CommandValue::Text { name, value } => {
                    encoder.byte(2);
                    encoder.string(name)?;
                    encoder.string(value)?;
                }
                CommandValue::ApprovedFile { name, path } => {
                    encoder.byte(3);
                    encoder.string(name)?;
                    encoder.string(path)?;
                }
                CommandValue::Url { name, value } => {
                    encoder.byte(4);
                    encoder.string(name)?;
                    encoder.string(value)?;
                }
            }
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, COMMAND_REQUEST_MAGIC)?;
        let command_id = decoder.string()?;
        let count = usize::from(decoder.u16()?);
        if count > MAX_COMMAND_VALUES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut names = BTreeSet::new();
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let value = match decoder.byte()? {
                0 => CommandValue::Integer {
                    name: decoder.string()?,
                    value: i64::from_le_bytes(decoder.array()?),
                },
                1 => CommandValue::FixedEnum {
                    name: decoder.string()?,
                    value: decoder.string()?,
                },
                2 => CommandValue::Text {
                    name: decoder.string()?,
                    value: decoder.string()?,
                },
                3 => CommandValue::ApprovedFile {
                    name: decoder.string()?,
                    path: decoder.string()?,
                },
                4 => CommandValue::Url {
                    name: decoder.string()?,
                    value: decoder.string()?,
                },
                _ => return Err(SchemaError::Malformed),
            };
            if !names.insert(value.name().to_owned()) {
                return Err(SchemaError::Malformed);
            }
            values.push(value);
        }
        decoder.finish()?;
        Ok(Self { command_id, values })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandOpened {
    pub resource_id: u64,
}

impl CommandOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(COMMAND_OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, COMMAND_OPENED_MAGIC)?;
        let resource_id = decoder.u64()?;
        decoder.finish()?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self { resource_id })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stdout_bytes: u64,
        stderr_bytes: u64,
    },
}

impl CommandEvent {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(COMMAND_EVENT_MAGIC);
        match self {
            Self::Stdout(bytes) | Self::Stderr(bytes) => {
                if bytes.is_empty() || bytes.len() > MAX_COMMAND_OUTPUT_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                encoder.byte(u8::from(matches!(self, Self::Stderr(_))));
                encoder.u64(bytes.len() as u64);
                encoder.bytes.extend_from_slice(bytes);
            }
            Self::Exited {
                exit_code,
                signal,
                stdout_bytes,
                stderr_bytes,
            } => {
                if exit_code.is_some() == signal.is_some() {
                    return Err(SchemaError::Malformed);
                }
                encoder.byte(2);
                encode_optional_i32(&mut encoder, *exit_code);
                encode_optional_i32(&mut encoder, *signal);
                encoder.u64(*stdout_bytes);
                encoder.u64(*stderr_bytes);
            }
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, COMMAND_EVENT_MAGIC)?;
        let event = match decoder.byte()? {
            stream @ (0 | 1) => {
                let length =
                    usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
                if length == 0 || length > MAX_COMMAND_OUTPUT_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                let bytes = decoder.take(length)?.to_vec();
                if stream == 0 {
                    Self::Stdout(bytes)
                } else {
                    Self::Stderr(bytes)
                }
            }
            2 => {
                let exit_code = decode_optional_i32(&mut decoder)?;
                let signal = decode_optional_i32(&mut decoder)?;
                if exit_code.is_some() == signal.is_some() {
                    return Err(SchemaError::Malformed);
                }
                Self::Exited {
                    exit_code,
                    signal,
                    stdout_bytes: decoder.u64()?,
                    stderr_bytes: decoder.u64()?,
                }
            }
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(event)
    }
}

fn encode_optional_i32(encoder: &mut Encoder, value: Option<i32>) {
    encoder.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        encoder.bytes.extend_from_slice(&value.to_le_bytes());
    }
}

fn decode_optional_i32(decoder: &mut Decoder<'_>) -> Result<Option<i32>, SchemaError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => Ok(Some(i32::from_le_bytes(decoder.array()?))),
        _ => Err(SchemaError::Malformed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpRequestMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    pub method: HttpRequestMethod,
    pub url: String,
    /// The only caller-controlled request headers in the inline v1 adapter.
    /// Credential-bearing and hop-by-hop headers cannot be represented.
    pub accept: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.body.len() > MAX_HTTP_INLINE_BODY_BYTES as usize {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(HTTP_REQUEST_MAGIC);
        encoder.byte(http_method_code(self.method));
        encoder.string(&self.url)?;
        encoder.optional_string(self.accept.as_deref())?;
        encoder.optional_string(self.content_type.as_deref())?;
        encoder.u64(self.body.len() as u64);
        encoder.bytes.extend_from_slice(&self.body);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, HTTP_REQUEST_MAGIC)?;
        let method = decode_http_method(decoder.byte()?)?;
        let url = decoder.string()?;
        let accept = decoder.optional_string()?;
        let content_type = decoder.optional_string()?;
        let length = usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
        if length > MAX_HTTP_INLINE_BODY_BYTES as usize {
            return Err(SchemaError::LimitExceeded);
        }
        let body = decoder.take(length)?.to_vec();
        decoder.finish()?;
        Ok(Self {
            method,
            url,
            accept,
            content_type,
            body,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if !(100..=599).contains(&self.status)
            || self.body.len() > MAX_HTTP_INLINE_BODY_BYTES as usize
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(HTTP_RESPONSE_MAGIC);
        encoder.u16(self.status);
        encoder.string(&self.final_url)?;
        encoder.optional_string(self.content_type.as_deref())?;
        encoder.optional_string(self.etag.as_deref())?;
        encoder.u64(self.body.len() as u64);
        encoder.bytes.extend_from_slice(&self.body);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, HTTP_RESPONSE_MAGIC)?;
        let status = decoder.u16()?;
        if !(100..=599).contains(&status) {
            return Err(SchemaError::Malformed);
        }
        let final_url = decoder.string()?;
        let content_type = decoder.optional_string()?;
        let etag = decoder.optional_string()?;
        let length = usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
        if length > MAX_HTTP_INLINE_BODY_BYTES as usize {
            return Err(SchemaError::LimitExceeded);
        }
        let body = decoder.take(length)?.to_vec();
        decoder.finish()?;
        Ok(Self {
            status,
            final_url,
            content_type,
            etag,
            body,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpStreamOpened {
    pub resource_id: u64,
}

impl HttpStreamOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(HTTP_STREAM_OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, HTTP_STREAM_OPENED_MAGIC)?;
        let resource_id = decoder.u64()?;
        decoder.finish()?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self { resource_id })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpStreamEvent {
    Metadata {
        status: u16,
        final_url: String,
        content_type: Option<String>,
        etag: Option<String>,
    },
    Chunk(Vec<u8>),
    Complete {
        total_bytes: u64,
    },
}

impl HttpStreamEvent {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(HTTP_STREAM_EVENT_MAGIC);
        match self {
            Self::Metadata {
                status,
                final_url,
                content_type,
                etag,
            } => {
                if !(100..=599).contains(status) {
                    return Err(SchemaError::Malformed);
                }
                encoder.byte(0);
                encoder.u16(*status);
                encoder.string(final_url)?;
                encoder.optional_string(content_type.as_deref())?;
                encoder.optional_string(etag.as_deref())?;
            }
            Self::Chunk(bytes) => {
                if bytes.is_empty() || bytes.len() > MAX_HTTP_STREAM_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                encoder.byte(1);
                encoder.u64(bytes.len() as u64);
                encoder.bytes.extend_from_slice(bytes);
            }
            Self::Complete { total_bytes } => {
                encoder.byte(2);
                encoder.u64(*total_bytes);
            }
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, HTTP_STREAM_EVENT_MAGIC)?;
        let event = match decoder.byte()? {
            0 => {
                let status = decoder.u16()?;
                if !(100..=599).contains(&status) {
                    return Err(SchemaError::Malformed);
                }
                Self::Metadata {
                    status,
                    final_url: decoder.string()?,
                    content_type: decoder.optional_string()?,
                    etag: decoder.optional_string()?,
                }
            }
            1 => {
                let length =
                    usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
                if length == 0 || length > MAX_HTTP_STREAM_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                Self::Chunk(decoder.take(length)?.to_vec())
            }
            2 => Self::Complete {
                total_bytes: decoder.u64()?,
            },
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(event)
    }
}

fn http_method_code(method: HttpRequestMethod) -> u8 {
    match method {
        HttpRequestMethod::Get => 0,
        HttpRequestMethod::Head => 1,
        HttpRequestMethod::Post => 2,
        HttpRequestMethod::Put => 3,
        HttpRequestMethod::Patch => 4,
        HttpRequestMethod::Delete => 5,
    }
}

fn decode_http_method(code: u8) -> Result<HttpRequestMethod, SchemaError> {
    match code {
        0 => Ok(HttpRequestMethod::Get),
        1 => Ok(HttpRequestMethod::Head),
        2 => Ok(HttpRequestMethod::Post),
        3 => Ok(HttpRequestMethod::Put),
        4 => Ok(HttpRequestMethod::Patch),
        5 => Ok(HttpRequestMethod::Delete),
        _ => Err(SchemaError::Malformed),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemReadFile {
    pub mount: String,
    /// Slash-separated path relative to the granted mount. An empty path is
    /// invalid for reads and represents the mount root for directory lists.
    pub path: String,
    pub offset: u64,
    pub maximum_bytes: u64,
}

impl FilesystemReadFile {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.maximum_bytes == 0 || self.maximum_bytes > MAX_FILE_CHUNK_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_READ_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.u64(self.offset);
        encoder.u64(self.maximum_bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_READ_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            path: decoder.string()?,
            offset: decoder.u64()?,
            maximum_bytes: decoder.u64()?,
        };
        decoder.finish()?;
        if value.maximum_bytes == 0 || value.maximum_bytes > MAX_FILE_CHUNK_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemReadStream {
    pub mount: String,
    pub path: String,
    pub offset: u64,
    pub maximum_bytes: u64,
}

impl FilesystemReadStream {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.maximum_bytes == 0 || self.maximum_bytes > MAX_FILE_STREAM_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_STREAM_REQUEST_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.u64(self.offset);
        encoder.u64(self.maximum_bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_STREAM_REQUEST_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            path: decoder.string()?,
            offset: decoder.u64()?,
            maximum_bytes: decoder.u64()?,
        };
        decoder.finish()?;
        if value.maximum_bytes == 0 || value.maximum_bytes > MAX_FILE_STREAM_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemStreamOpened {
    pub resource_id: u64,
}

impl FilesystemStreamOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(FILE_STREAM_OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_STREAM_OPENED_MAGIC)?;
        let resource_id = decoder.u64()?;
        decoder.finish()?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self { resource_id })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesystemStreamEvent {
    Metadata {
        offset: u64,
        total_size: u64,
    },
    Chunk {
        offset: u64,
        bytes: Vec<u8>,
    },
    Complete {
        end_offset: u64,
        total_bytes: u64,
        eof: bool,
    },
}

impl FilesystemStreamEvent {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(FILE_STREAM_EVENT_MAGIC);
        match self {
            Self::Metadata { offset, total_size } => {
                encoder.byte(0);
                encoder.u64(*offset);
                encoder.u64(*total_size);
            }
            Self::Chunk { offset, bytes } => {
                if bytes.is_empty() || bytes.len() > MAX_FILE_STREAM_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                encoder.byte(1);
                encoder.u64(*offset);
                encoder.u64(bytes.len() as u64);
                encoder.bytes.extend_from_slice(bytes);
            }
            Self::Complete {
                end_offset,
                total_bytes,
                eof,
            } => {
                encoder.byte(2);
                encoder.u64(*end_offset);
                encoder.u64(*total_bytes);
                encoder.byte(u8::from(*eof));
            }
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_STREAM_EVENT_MAGIC)?;
        let event = match decoder.byte()? {
            0 => Self::Metadata {
                offset: decoder.u64()?,
                total_size: decoder.u64()?,
            },
            1 => {
                let offset = decoder.u64()?;
                let length =
                    usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
                if length == 0 || length > MAX_FILE_STREAM_CHUNK_BYTES {
                    return Err(SchemaError::LimitExceeded);
                }
                Self::Chunk {
                    offset,
                    bytes: decoder.take(length)?.to_vec(),
                }
            }
            2 => Self::Complete {
                end_offset: decoder.u64()?,
                total_bytes: decoder.u64()?,
                eof: match decoder.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(SchemaError::Malformed),
                },
            },
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(event)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemFileChunk {
    pub offset: u64,
    pub total_size: u64,
    pub eof: bool,
    pub bytes: Vec<u8>,
}

impl FilesystemFileChunk {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.bytes.len() > MAX_FILE_CHUNK_BYTES as usize {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(FILE_REPLY_MAGIC);
        encoder.u64(self.offset);
        encoder.u64(self.total_size);
        encoder.byte(u8::from(self.eof));
        encoder.u64(self.bytes.len() as u64);
        encoder.bytes.extend_from_slice(&self.bytes);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, FILE_REPLY_MAGIC)?;
        let offset = decoder.u64()?;
        let total_size = decoder.u64()?;
        let eof = match decoder.byte()? {
            0 => false,
            1 => true,
            _ => return Err(SchemaError::Malformed),
        };
        let length = usize::try_from(decoder.u64()?).map_err(|_| SchemaError::LimitExceeded)?;
        if length > MAX_FILE_CHUNK_BYTES as usize {
            return Err(SchemaError::LimitExceeded);
        }
        let bytes = decoder.take(length)?.to_vec();
        decoder.finish()?;
        Ok(Self {
            offset,
            total_size,
            eof,
            bytes,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemListDirectory {
    pub mount: String,
    pub path: String,
    pub maximum_entries: u16,
}

impl FilesystemListDirectory {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.maximum_entries == 0 || self.maximum_entries > MAX_DIRECTORY_ENTRIES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(DIRECTORY_LIST_MAGIC);
        encoder.string(&self.mount)?;
        encoder.string(&self.path)?;
        encoder.u16(self.maximum_entries);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, DIRECTORY_LIST_MAGIC)?;
        let value = Self {
            mount: decoder.string()?,
            path: decoder.string()?,
            maximum_entries: decoder.u16()?,
        };
        decoder.finish()?;
        if value.maximum_entries == 0 || value.maximum_entries > MAX_DIRECTORY_ENTRIES {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemEntryKind {
    RegularFile,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemEntry {
    pub name: String,
    pub kind: FilesystemEntryKind,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemDirectoryEntries {
    pub entries: Vec<FilesystemEntry>,
    pub truncated: bool,
}

impl FilesystemDirectoryEntries {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.entries.len() > usize::from(MAX_DIRECTORY_ENTRIES) {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(DIRECTORY_REPLY_MAGIC);
        encoder.byte(u8::from(self.truncated));
        encoder.u16(self.entries.len() as u16);
        for entry in &self.entries {
            encoder.string(&entry.name)?;
            encoder.byte(match entry.kind {
                FilesystemEntryKind::RegularFile => 0,
                FilesystemEntryKind::Directory => 1,
            });
            encoder.u64(entry.size);
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, DIRECTORY_REPLY_MAGIC)?;
        let truncated = match decoder.byte()? {
            0 => false,
            1 => true,
            _ => return Err(SchemaError::Malformed),
        };
        let count = usize::from(decoder.u16()?);
        if count > usize::from(MAX_DIRECTORY_ENTRIES) {
            return Err(SchemaError::LimitExceeded);
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let name = decoder.string()?;
            let kind = match decoder.byte()? {
                0 => FilesystemEntryKind::RegularFile,
                1 => FilesystemEntryKind::Directory,
                _ => return Err(SchemaError::Malformed),
            };
            entries.push(FilesystemEntry {
                name,
                kind,
                size: decoder.u64()?,
            });
        }
        decoder.finish()?;
        Ok(Self { entries, truncated })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DbusBus {
    Session,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DbusReplyKind {
    Unit,
    VariantString,
    VariantI64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DbusCall {
    pub bus: DbusBus,
    pub destination: String,
    pub path: String,
    pub interface: String,
    pub member: String,
    pub arguments: Vec<String>,
    pub reply: DbusReplyKind,
}

/// One exact signal subscription. The first production decoder supports the
/// standard `PropertiesChanged` body (`sa{sv}as`); the remaining fields are
/// still explicit so the supervisor can construct a least-authority match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DbusSubscription {
    pub bus: DbusBus,
    pub sender: String,
    pub path: String,
    pub interface: String,
    pub member: String,
    pub signature: String,
    pub argument_zero: Option<String>,
}

impl DbusSubscription {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(SUBSCRIPTION_MAGIC);
        encoder.byte(match self.bus {
            DbusBus::Session => 0,
            DbusBus::System => 1,
        });
        encoder.string(&self.sender)?;
        encoder.string(&self.path)?;
        encoder.string(&self.interface)?;
        encoder.string(&self.member)?;
        encoder.string(&self.signature)?;
        encoder.byte(u8::from(self.argument_zero.is_some()));
        if let Some(argument_zero) = &self.argument_zero {
            encoder.string(argument_zero)?;
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, SUBSCRIPTION_MAGIC)?;
        let bus = match decoder.byte()? {
            0 => DbusBus::Session,
            1 => DbusBus::System,
            _ => return Err(SchemaError::Malformed),
        };
        let sender = decoder.string()?;
        let path = decoder.string()?;
        let interface = decoder.string()?;
        let member = decoder.string()?;
        let signature = decoder.string()?;
        let argument_zero = match decoder.byte()? {
            0 => None,
            1 => Some(decoder.string()?),
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(Self {
            bus,
            sender,
            path,
            interface,
            member,
            signature,
            argument_zero,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DbusSubscriptionOpened {
    pub resource_id: u64,
}

impl DbusSubscriptionOpened {
    pub fn encode(self) -> Result<Vec<u8>, SchemaError> {
        if self.resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        let mut encoder = Encoder::new(OPENED_MAGIC);
        encoder.u64(self.resource_id);
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, OPENED_MAGIC)?;
        let resource_id = decoder.u64()?;
        decoder.finish()?;
        if resource_id == 0 {
            return Err(SchemaError::Malformed);
        }
        Ok(Self { resource_id })
    }
}

/// Typed values deliberately supported by the first signal adapter. File
/// descriptors, nested containers, object paths, and arbitrary variants never
/// cross into the component sandbox.
#[derive(Clone, Debug, PartialEq)]
pub enum DbusValue {
    U8(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DbusProperty {
    pub name: String,
    pub value: DbusValue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DbusPropertiesChanged {
    pub interface_name: String,
    pub changed_properties: Vec<DbusProperty>,
    pub invalidated_properties: Vec<String>,
}

impl DbusPropertiesChanged {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.changed_properties.len() > MAX_PROPERTIES
            || self.invalidated_properties.len() > MAX_PROPERTIES
        {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(SIGNAL_MAGIC);
        encoder.string(&self.interface_name)?;
        encoder.u16(self.changed_properties.len() as u16);
        for property in &self.changed_properties {
            encoder.string(&property.name)?;
            encode_value(&mut encoder, &property.value)?;
        }
        encoder.u16(self.invalidated_properties.len() as u16);
        for property in &self.invalidated_properties {
            encoder.string(property)?;
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, SIGNAL_MAGIC)?;
        let interface_name = decoder.string()?;
        let changed_count = usize::from(decoder.u16()?);
        if changed_count > MAX_PROPERTIES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut changed_properties = Vec::with_capacity(changed_count);
        for _ in 0..changed_count {
            changed_properties.push(DbusProperty {
                name: decoder.string()?,
                value: decode_value(&mut decoder)?,
            });
        }
        let invalidated_count = usize::from(decoder.u16()?);
        if invalidated_count > MAX_PROPERTIES {
            return Err(SchemaError::LimitExceeded);
        }
        let mut invalidated_properties = Vec::with_capacity(invalidated_count);
        for _ in 0..invalidated_count {
            invalidated_properties.push(decoder.string()?);
        }
        decoder.finish()?;
        Ok(Self {
            interface_name,
            changed_properties,
            invalidated_properties,
        })
    }
}

fn encode_value(encoder: &mut Encoder, value: &DbusValue) -> Result<(), SchemaError> {
    match value {
        DbusValue::U8(value) => {
            encoder.byte(0);
            encoder.byte(*value);
        }
        DbusValue::Bool(value) => {
            encoder.byte(1);
            encoder.byte(u8::from(*value));
        }
        DbusValue::I16(value) => {
            encoder.byte(2);
            encoder.bytes.extend_from_slice(&value.to_le_bytes());
        }
        DbusValue::U16(value) => {
            encoder.byte(3);
            encoder.bytes.extend_from_slice(&value.to_le_bytes());
        }
        DbusValue::I32(value) => {
            encoder.byte(4);
            encoder.bytes.extend_from_slice(&value.to_le_bytes());
        }
        DbusValue::U32(value) => {
            encoder.byte(5);
            encoder.bytes.extend_from_slice(&value.to_le_bytes());
        }
        DbusValue::I64(value) => {
            encoder.byte(6);
            encoder.bytes.extend_from_slice(&value.to_le_bytes());
        }
        DbusValue::U64(value) => {
            encoder.byte(7);
            encoder.u64(*value);
        }
        DbusValue::F64(value) if value.is_finite() => {
            encoder.byte(8);
            encoder.u64(value.to_bits());
        }
        DbusValue::F64(_) => return Err(SchemaError::Malformed),
        DbusValue::String(value) => {
            encoder.byte(9);
            encoder.string(value)?;
        }
    }
    Ok(())
}

fn decode_value(decoder: &mut Decoder<'_>) -> Result<DbusValue, SchemaError> {
    match decoder.byte()? {
        0 => Ok(DbusValue::U8(decoder.byte()?)),
        1 => match decoder.byte()? {
            0 => Ok(DbusValue::Bool(false)),
            1 => Ok(DbusValue::Bool(true)),
            _ => Err(SchemaError::Malformed),
        },
        2 => Ok(DbusValue::I16(i16::from_le_bytes(decoder.array()?))),
        3 => Ok(DbusValue::U16(u16::from_le_bytes(decoder.array()?))),
        4 => Ok(DbusValue::I32(i32::from_le_bytes(decoder.array()?))),
        5 => Ok(DbusValue::U32(u32::from_le_bytes(decoder.array()?))),
        6 => Ok(DbusValue::I64(i64::from_le_bytes(decoder.array()?))),
        7 => Ok(DbusValue::U64(decoder.u64()?)),
        8 => {
            let value = f64::from_bits(decoder.u64()?);
            value
                .is_finite()
                .then_some(DbusValue::F64(value))
                .ok_or(SchemaError::Malformed)
        }
        9 => Ok(DbusValue::String(decoder.string()?)),
        _ => Err(SchemaError::Malformed),
    }
}

impl DbusCall {
    pub fn signature(&self) -> String {
        "s".repeat(self.arguments.len())
    }

    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        if self.arguments.len() > MAX_ARGUMENTS {
            return Err(SchemaError::LimitExceeded);
        }
        let mut encoder = Encoder::new(CALL_MAGIC);
        encoder.byte(match self.bus {
            DbusBus::Session => 0,
            DbusBus::System => 1,
        });
        encoder.string(&self.destination)?;
        encoder.string(&self.path)?;
        encoder.string(&self.interface)?;
        encoder.string(&self.member)?;
        encoder.byte(match self.reply {
            DbusReplyKind::Unit => 0,
            DbusReplyKind::VariantString => 1,
            DbusReplyKind::VariantI64 => 2,
        });
        encoder.u16(self.arguments.len() as u16);
        for argument in &self.arguments {
            encoder.string(argument)?;
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, CALL_MAGIC)?;
        let bus = match decoder.byte()? {
            0 => DbusBus::Session,
            1 => DbusBus::System,
            _ => return Err(SchemaError::Malformed),
        };
        let destination = decoder.string()?;
        let path = decoder.string()?;
        let interface = decoder.string()?;
        let member = decoder.string()?;
        let reply = match decoder.byte()? {
            0 => DbusReplyKind::Unit,
            1 => DbusReplyKind::VariantString,
            2 => DbusReplyKind::VariantI64,
            _ => return Err(SchemaError::Malformed),
        };
        let argument_count = usize::from(decoder.u16()?);
        if argument_count > MAX_ARGUMENTS {
            return Err(SchemaError::LimitExceeded);
        }
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(decoder.string()?);
        }
        decoder.finish()?;
        Ok(Self {
            bus,
            destination,
            path,
            interface,
            member,
            arguments,
            reply,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DbusReply {
    Unit,
    String(String),
    I64(i64),
}

impl DbusReply {
    pub fn encode(&self) -> Result<Vec<u8>, SchemaError> {
        let mut encoder = Encoder::new(REPLY_MAGIC);
        match self {
            Self::Unit => encoder.byte(0),
            Self::String(value) => {
                encoder.byte(1);
                encoder.string(value)?;
            }
            Self::I64(value) => {
                encoder.byte(2);
                encoder.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        encoder.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchemaError> {
        let mut decoder = Decoder::new(bytes, REPLY_MAGIC)?;
        let reply = match decoder.byte()? {
            0 => Self::Unit,
            1 => Self::String(decoder.string()?),
            2 => Self::I64(i64::from_le_bytes(decoder.array()?)),
            _ => return Err(SchemaError::Malformed),
        };
        decoder.finish()?;
        Ok(reply)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaError {
    Malformed,
    LimitExceeded,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Malformed => "malformed capability payload",
            Self::LimitExceeded => "capability payload exceeds its limit",
        })
    }
}

impl Error for SchemaError {}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(magic: [u8; 8]) -> Self {
        Self {
            bytes: magic.to_vec(),
        }
    }

    fn byte(&mut self, value: u8) {
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

    fn string(&mut self, value: &str) -> Result<(), SchemaError> {
        if value.len() > MAX_STRING_BYTES || value.len() > usize::from(u16::MAX) {
            return Err(SchemaError::LimitExceeded);
        }
        self.u16(value.len() as u16);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn optional_string(&mut self, value: Option<&str>) -> Result<(), SchemaError> {
        self.byte(u8::from(value.is_some()));
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn blob(&mut self, value: &[u8]) -> Result<(), SchemaError> {
        let length = u32::try_from(value.len()).map_err(|_| SchemaError::LimitExceeded)?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, SchemaError> {
        if self.bytes.len() > MAX_PAYLOAD_BYTES {
            Err(SchemaError::LimitExceeded)
        } else {
            Ok(self.bytes)
        }
    }
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], magic: [u8; 8]) -> Result<Self, SchemaError> {
        if bytes.len() > MAX_PAYLOAD_BYTES || !bytes.starts_with(&magic) {
            return Err(if bytes.len() > MAX_PAYLOAD_BYTES {
                SchemaError::LimitExceeded
            } else {
                SchemaError::Malformed
            });
        }
        Ok(Self {
            remaining: &bytes[magic.len()..],
        })
    }

    fn byte(&mut self) -> Result<u8, SchemaError> {
        let (&value, remaining) = self.remaining.split_first().ok_or(SchemaError::Malformed)?;
        self.remaining = remaining;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, SchemaError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, SchemaError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, SchemaError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SchemaError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(N)
            .ok_or(SchemaError::Malformed)?;
        self.remaining = remaining;
        value.try_into().map_err(|_| SchemaError::Malformed)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SchemaError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(length)
            .ok_or(SchemaError::Malformed)?;
        self.remaining = remaining;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, SchemaError> {
        let length = usize::from(self.u16()?);
        if length > MAX_STRING_BYTES {
            return Err(SchemaError::LimitExceeded);
        }
        let (value, remaining) = self
            .remaining
            .split_at_checked(length)
            .ok_or(SchemaError::Malformed)?;
        self.remaining = remaining;
        std::str::from_utf8(value)
            .map(str::to_owned)
            .map_err(|_| SchemaError::Malformed)
    }

    fn optional_string(&mut self) -> Result<Option<String>, SchemaError> {
        match self.byte()? {
            0 => Ok(None),
            1 => self.string().map(Some),
            _ => Err(SchemaError::Malformed),
        }
    }

    fn blob(&mut self, maximum: usize) -> Result<Vec<u8>, SchemaError> {
        let length = usize::try_from(self.u32()?).map_err(|_| SchemaError::LimitExceeded)?;
        if length > maximum {
            return Err(SchemaError::LimitExceeded);
        }
        Ok(self.take(length)?.to_vec())
    }

    fn finish(self) -> Result<(), SchemaError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(SchemaError::Malformed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise_all_decoders(bytes: &[u8]) {
        let _ = ContextReadRequest::decode(bytes);
        let _ = ContextSnapshot::decode(bytes);
        let _ = ContextSubscriptionOpened::decode(bytes);
        let _ = ClipboardReadRequest::decode(bytes);
        let _ = ClipboardWriteRequest::decode(bytes);
        let _ = ClipboardValue::decode(bytes);
        let _ = SecretReadRequest::decode(bytes);
        let _ = SecretValue::decode(bytes);
        let _ = LocalConnect::decode(bytes);
        let _ = LocalConnectionOpened::decode(bytes);
        let _ = LocalSendFrame::decode(bytes);
        let _ = LocalFrameEvent::decode(bytes);
        let _ = NotificationSend::decode(bytes);
        let _ = NotificationRemove::decode(bytes);
        let _ = UriOpenRequest::decode(bytes);
        let _ = FilesystemWriteFile::decode(bytes);
        let _ = FilesystemWriteStream::decode(bytes);
        let _ = FilesystemWriteStreamOpened::decode(bytes);
        let _ = FilesystemWriteStreamChunk::decode(bytes);
        let _ = FilesystemWriteStreamCommit::decode(bytes);
        let _ = FilesystemPath::decode(bytes);
        let _ = FilesystemRename::decode(bytes);
        let _ = FilesystemMutationResult::decode(bytes);
        let _ = CommandRunRequest::decode(bytes);
        let _ = CommandOpened::decode(bytes);
        let _ = CommandEvent::decode(bytes);
        let _ = HttpRequest::decode(bytes);
        let _ = HttpResponse::decode(bytes);
        let _ = HttpStreamOpened::decode(bytes);
        let _ = HttpStreamEvent::decode(bytes);
        let _ = FilesystemReadFile::decode(bytes);
        let _ = FilesystemReadStream::decode(bytes);
        let _ = FilesystemStreamOpened::decode(bytes);
        let _ = FilesystemStreamEvent::decode(bytes);
        let _ = FilesystemFileChunk::decode(bytes);
        let _ = FilesystemListDirectory::decode(bytes);
        let _ = FilesystemDirectoryEntries::decode(bytes);
        let _ = DbusCall::decode(bytes);
        let _ = DbusReply::decode(bytes);
        let _ = DbusSubscription::decode(bytes);
        let _ = DbusSubscriptionOpened::decode(bytes);
        let _ = DbusPropertiesChanged::decode(bytes);
    }

    #[test]
    fn deterministic_adversarial_corpus_never_panics_or_allocates_from_untrusted_lengths() {
        const MAGICS: &[[u8; 8]] = &[
            CONTEXT_READ_MAGIC,
            CONTEXT_SNAPSHOT_MAGIC,
            CONTEXT_OPENED_MAGIC,
            CLIPBOARD_READ_MAGIC,
            CLIPBOARD_WRITE_MAGIC,
            CLIPBOARD_VALUE_MAGIC,
            SECRET_READ_MAGIC,
            SECRET_VALUE_MAGIC,
            LOCAL_CONNECT_MAGIC,
            LOCAL_OPENED_MAGIC,
            LOCAL_SEND_MAGIC,
            LOCAL_EVENT_MAGIC,
            FILE_READ_MAGIC,
            FILE_REPLY_MAGIC,
            FILE_STREAM_REQUEST_MAGIC,
            FILE_STREAM_OPENED_MAGIC,
            FILE_STREAM_EVENT_MAGIC,
            DIRECTORY_LIST_MAGIC,
            DIRECTORY_REPLY_MAGIC,
            FILE_WRITE_MAGIC,
            FILE_PATH_MAGIC,
            FILE_RENAME_MAGIC,
            FILE_MUTATION_MAGIC,
            FILE_WRITE_STREAM_MAGIC,
            FILE_WRITE_STREAM_OPENED_MAGIC,
            FILE_WRITE_STREAM_CHUNK_MAGIC,
            FILE_WRITE_STREAM_COMMIT_MAGIC,
            URI_OPEN_MAGIC,
            NOTIFICATION_SEND_MAGIC,
            NOTIFICATION_REMOVE_MAGIC,
            HTTP_REQUEST_MAGIC,
            HTTP_RESPONSE_MAGIC,
            HTTP_STREAM_OPENED_MAGIC,
            HTTP_STREAM_EVENT_MAGIC,
            COMMAND_REQUEST_MAGIC,
            COMMAND_OPENED_MAGIC,
            COMMAND_EVENT_MAGIC,
            CALL_MAGIC,
            REPLY_MAGIC,
            SUBSCRIPTION_MAGIC,
            OPENED_MAGIC,
            SIGNAL_MAGIC,
        ];
        let mut random = 0x9e37_79b9_7f4a_7c15_u64;
        for index in 0..4096 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let length = match index % 127 {
                0 => MAX_PAYLOAD_BYTES + 1,
                1 => MAX_PAYLOAD_BYTES,
                _ => (random as usize) % 2048,
            };
            let mut bytes = vec![0_u8; length];
            for byte in &mut bytes {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                *byte = random as u8;
            }
            if bytes.len() >= 8 {
                bytes[..8].copy_from_slice(&MAGICS[index % MAGICS.len()]);
            }
            exercise_all_decoders(&bytes);
        }
    }

    #[test]
    fn call_and_reply_round_trip_without_ambiguous_trailing_data() {
        let call = DbusCall {
            bus: DbusBus::Session,
            destination: "org.mpris.MediaPlayer2.demo".into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: "org.freedesktop.DBus.Properties".into(),
            member: "Get".into(),
            arguments: vec![
                "org.mpris.MediaPlayer2.Player".into(),
                "PlaybackStatus".into(),
            ],
            reply: DbusReplyKind::VariantString,
        };
        let encoded = call.encode().unwrap();
        assert_eq!(DbusCall::decode(&encoded), Ok(call));

        for reply in [
            DbusReply::Unit,
            DbusReply::String("Playing".into()),
            DbusReply::I64(42),
        ] {
            assert_eq!(DbusReply::decode(&reply.encode().unwrap()), Ok(reply));
        }

        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(DbusCall::decode(&trailing), Err(SchemaError::Malformed));
    }

    #[test]
    fn malformed_and_unbounded_payloads_fail_closed() {
        assert_eq!(DbusCall::decode(b"not-a-call"), Err(SchemaError::Malformed));
        assert_eq!(
            DbusCall::decode(&vec![0; MAX_PAYLOAD_BYTES + 1]),
            Err(SchemaError::LimitExceeded)
        );
        let call = DbusCall {
            bus: DbusBus::Session,
            destination: "x".repeat(MAX_STRING_BYTES + 1),
            path: "/x".into(),
            interface: "x.y".into(),
            member: "X".into(),
            arguments: Vec::new(),
            reply: DbusReplyKind::Unit,
        };
        assert_eq!(call.encode(), Err(SchemaError::LimitExceeded));
    }

    #[test]
    fn filesystem_mutation_payloads_are_typed_and_bounded() {
        let write = FilesystemWriteFile {
            mount: "documents".into(),
            path: "notes/today.txt".into(),
            bytes: b"hello".to_vec(),
        };
        assert_eq!(
            FilesystemWriteFile::decode(&write.encode().unwrap()),
            Ok(write)
        );

        let path = FilesystemPath {
            mount: "documents".into(),
            path: "notes".into(),
        };
        assert_eq!(FilesystemPath::decode(&path.encode().unwrap()), Ok(path));

        let rename = FilesystemRename {
            mount: "documents".into(),
            source: "old.txt".into(),
            destination: "archive/old.txt".into(),
        };
        assert_eq!(
            FilesystemRename::decode(&rename.encode().unwrap()),
            Ok(rename)
        );

        let stream = FilesystemWriteStream {
            mount: "documents".into(),
            path: "large.bin".into(),
            expected_bytes: 32 * 1024,
        };
        assert_eq!(
            FilesystemWriteStream::decode(&stream.encode().unwrap()),
            Ok(stream)
        );
        let opened = FilesystemWriteStreamOpened {
            resource_id: 7,
            maximum_chunk_bytes: MAX_FILE_WRITE_STREAM_CHUNK_BYTES as u32,
        };
        assert_eq!(
            FilesystemWriteStreamOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        let stream_chunk = FilesystemWriteStreamChunk {
            resource_id: 7,
            offset: 12,
            bytes: b"chunk".to_vec(),
        };
        assert_eq!(
            FilesystemWriteStreamChunk::decode(&stream_chunk.encode().unwrap()),
            Ok(stream_chunk)
        );
        let commit = FilesystemWriteStreamCommit { resource_id: 7 };
        assert_eq!(
            FilesystemWriteStreamCommit::decode(&commit.encode().unwrap()),
            Ok(commit)
        );

        for result in [
            FilesystemMutationResult {
                bytes_written: 5,
                resulting_size: Some(10),
            },
            FilesystemMutationResult {
                bytes_written: 0,
                resulting_size: None,
            },
        ] {
            assert_eq!(
                FilesystemMutationResult::decode(&result.encode().unwrap()),
                Ok(result)
            );
        }

        let oversized = FilesystemWriteFile {
            mount: "documents".into(),
            path: "large".into(),
            bytes: vec![0; MAX_FILE_WRITE_BYTES + 1],
        };
        assert_eq!(oversized.encode(), Err(SchemaError::LimitExceeded));
        assert_eq!(
            FilesystemWriteStreamChunk {
                resource_id: 7,
                offset: 0,
                bytes: vec![0; MAX_FILE_WRITE_STREAM_CHUNK_BYTES + 1],
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        let mut trailing = FilesystemPath {
            mount: "documents".into(),
            path: "file".into(),
        }
        .encode()
        .unwrap();
        trailing.push(0);
        assert_eq!(
            FilesystemPath::decode(&trailing),
            Err(SchemaError::Malformed)
        );
    }

    #[test]
    fn uri_open_payload_is_bounded_and_strict() {
        let request = UriOpenRequest {
            uri: "https://example.com/docs".into(),
        };
        assert_eq!(
            UriOpenRequest::decode(&request.encode().unwrap()),
            Ok(request)
        );
        assert_eq!(
            UriOpenRequest {
                uri: "x".repeat(MAX_URI_BYTES + 1),
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        let mut trailing = UriOpenRequest {
            uri: "mailto:test@example.com".into(),
        }
        .encode()
        .unwrap();
        trailing.push(0);
        assert_eq!(
            UriOpenRequest::decode(&trailing),
            Err(SchemaError::Malformed)
        );
    }

    #[test]
    fn local_connection_payloads_are_framed_bounded_and_strict() {
        let connect = LocalConnect {
            endpoint: "music-player".into(),
            protocol: "mpris.bridge.v1".into(),
        };
        assert_eq!(
            LocalConnect::decode(&connect.encode().unwrap()),
            Ok(connect)
        );

        let opened = LocalConnectionOpened {
            resource_id: 8,
            maximum_frame_bytes: MAX_LOCAL_FRAME_BYTES as u32,
        };
        assert_eq!(
            LocalConnectionOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        let send = LocalSendFrame {
            resource_id: 8,
            bytes: b"request".to_vec(),
        };
        assert_eq!(LocalSendFrame::decode(&send.encode().unwrap()), Ok(send));
        let event = LocalFrameEvent {
            bytes: b"response".to_vec(),
        };
        assert_eq!(LocalFrameEvent::decode(&event.encode().unwrap()), Ok(event));

        assert_eq!(
            LocalSendFrame {
                resource_id: 8,
                bytes: vec![0; MAX_LOCAL_FRAME_BYTES + 1],
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        assert_eq!(
            LocalFrameEvent { bytes: Vec::new() }.encode(),
            Err(SchemaError::Malformed)
        );
        let mut trailing = LocalConnect {
            endpoint: "music-player".into(),
            protocol: "mpris.bridge.v1".into(),
        }
        .encode()
        .unwrap();
        trailing.push(0);
        assert_eq!(LocalConnect::decode(&trailing), Err(SchemaError::Malformed));
    }

    #[test]
    fn secret_payloads_are_inline_bounded_and_strict() {
        let request = SecretReadRequest {
            logical_name: "github-token".into(),
        };
        assert_eq!(
            SecretReadRequest::decode(&request.encode().unwrap()),
            Ok(request)
        );
        let value = SecretValue {
            bytes: b"not-logged".to_vec(),
            content_type: "text/plain; charset=utf8".into(),
        };
        assert_eq!(SecretValue::decode(&value.encode().unwrap()), Ok(value));
        assert_eq!(
            SecretValue {
                bytes: vec![0; MAX_SECRET_BYTES + 1],
                content_type: "application/octet-stream".into(),
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        let mut trailing = SecretReadRequest {
            logical_name: "token".into(),
        }
        .encode()
        .unwrap();
        trailing.push(0);
        assert_eq!(
            SecretReadRequest::decode(&trailing),
            Err(SchemaError::Malformed)
        );
    }

    #[test]
    fn clipboard_payloads_are_single_mime_inline_bounded_and_strict() {
        let read = ClipboardReadRequest {
            mime_type: "text/plain;charset=utf-8".into(),
        };
        assert_eq!(
            ClipboardReadRequest::decode(&read.encode().unwrap()),
            Ok(read)
        );
        let write = ClipboardWriteRequest {
            mime_type: "image/png".into(),
            bytes: b"png".to_vec(),
        };
        assert_eq!(
            ClipboardWriteRequest::decode(&write.encode().unwrap()),
            Ok(write)
        );
        let value = ClipboardValue {
            mime_type: "image/png".into(),
            bytes: b"png".to_vec(),
        };
        assert_eq!(ClipboardValue::decode(&value.encode().unwrap()), Ok(value));
        assert_eq!(
            ClipboardWriteRequest {
                mime_type: "application/octet-stream".into(),
                bytes: vec![0; MAX_CLIPBOARD_BYTES + 1],
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        assert_eq!(
            ClipboardReadRequest {
                mime_type: "x/".to_owned() + &"y".repeat(MAX_CLIPBOARD_MIME_BYTES),
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        let mut trailing = ClipboardReadRequest {
            mime_type: "text/plain".into(),
        }
        .encode()
        .unwrap();
        trailing.push(0);
        assert_eq!(
            ClipboardReadRequest::decode(&trailing),
            Err(SchemaError::Malformed)
        );
    }

    #[test]
    fn context_payloads_are_sorted_unique_bounded_and_strict() {
        let read = ContextReadRequest {
            facts: BTreeSet::from(["application.id".into(), "workspace.id".into()]),
        };
        assert_eq!(
            ContextReadRequest::decode(&read.encode().unwrap()),
            Ok(read)
        );
        let snapshot = ContextSnapshot {
            generation: 7,
            facts: vec![
                ContextFact {
                    key: "application.id".into(),
                    value: ContextFactValue::Text("terminal".into()),
                },
                ContextFact {
                    key: "power.on-battery".into(),
                    value: ContextFactValue::Boolean(true),
                },
            ],
        };
        assert_eq!(
            ContextSnapshot::decode(&snapshot.encode().unwrap()),
            Ok(snapshot.clone())
        );
        let opened = ContextSubscriptionOpened {
            resource_id: 9,
            snapshot,
        };
        assert_eq!(
            ContextSubscriptionOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        assert_eq!(
            ContextSnapshot {
                generation: 1,
                facts: vec![ContextFact {
                    key: "application.id".into(),
                    value: ContextFactValue::Text("x".repeat(MAX_CONTEXT_VALUE_BYTES + 1)),
                }],
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
        assert_eq!(
            ContextReadRequest {
                facts: BTreeSet::new()
            }
            .encode(),
            Err(SchemaError::Malformed)
        );
    }

    #[test]
    fn notification_payloads_are_bounded_and_typed() {
        let send = NotificationSend {
            id: "build".into(),
            category: "status".into(),
            urgency: NotificationUrgencyValue::Normal,
            title: "Complete".into(),
            body: "All checks passed".into(),
        };
        assert_eq!(NotificationSend::decode(&send.encode().unwrap()), Ok(send));
        let remove = NotificationRemove { id: "build".into() };
        assert_eq!(
            NotificationRemove::decode(&remove.encode().unwrap()),
            Ok(remove)
        );
        assert_eq!(
            NotificationSend {
                id: "build".into(),
                category: "status".into(),
                urgency: NotificationUrgencyValue::Low,
                title: "x".repeat(MAX_NOTIFICATION_TITLE_BYTES + 1),
                body: String::new(),
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
    }

    #[test]
    fn subscription_open_and_typed_properties_round_trip() {
        let subscription = DbusSubscription {
            bus: DbusBus::Session,
            sender: "org.mpris.MediaPlayer2.playerctld".into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: "org.freedesktop.DBus.Properties".into(),
            member: "PropertiesChanged".into(),
            signature: "sa{sv}as".into(),
            argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
        };
        assert_eq!(
            DbusSubscription::decode(&subscription.encode().unwrap()),
            Ok(subscription)
        );
        let opened = DbusSubscriptionOpened { resource_id: 42 };
        assert_eq!(
            DbusSubscriptionOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        let event = DbusPropertiesChanged {
            interface_name: "org.mpris.MediaPlayer2.Player".into(),
            changed_properties: vec![DbusProperty {
                name: "PlaybackStatus".into(),
                value: DbusValue::String("Playing".into()),
            }],
            invalidated_properties: vec!["Metadata".into()],
        };
        assert_eq!(
            DbusPropertiesChanged::decode(&event.encode().unwrap()),
            Ok(event)
        );
    }

    #[test]
    fn filesystem_payloads_are_bounded_and_round_trip() {
        let read = FilesystemReadFile {
            mount: "gallery".into(),
            path: "album/image.png".into(),
            offset: 32,
            maximum_bytes: 4096,
        };
        assert_eq!(
            FilesystemReadFile::decode(&read.encode().unwrap()),
            Ok(read)
        );
        let chunk = FilesystemFileChunk {
            offset: 32,
            total_size: 35,
            eof: true,
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            FilesystemFileChunk::decode(&chunk.encode().unwrap()),
            Ok(chunk)
        );
        let list = FilesystemListDirectory {
            mount: "gallery".into(),
            path: String::new(),
            maximum_entries: 16,
        };
        assert_eq!(
            FilesystemListDirectory::decode(&list.encode().unwrap()),
            Ok(list)
        );
        let entries = FilesystemDirectoryEntries {
            entries: vec![FilesystemEntry {
                name: "image.png".into(),
                kind: FilesystemEntryKind::RegularFile,
                size: 3,
            }],
            truncated: false,
        };
        assert_eq!(
            FilesystemDirectoryEntries::decode(&entries.encode().unwrap()),
            Ok(entries)
        );

        let oversized = FilesystemReadFile {
            maximum_bytes: MAX_FILE_CHUNK_BYTES + 1,
            ..FilesystemReadFile {
                mount: "gallery".into(),
                path: "image.png".into(),
                offset: 0,
                maximum_bytes: 1,
            }
        };
        assert_eq!(oversized.encode(), Err(SchemaError::LimitExceeded));
    }

    #[test]
    fn filesystem_stream_payloads_are_typed_and_bounded() {
        let request = FilesystemReadStream {
            mount: "gallery".into(),
            path: "large.raw".into(),
            offset: 17,
            maximum_bytes: 1024 * 1024,
        };
        assert_eq!(
            FilesystemReadStream::decode(&request.encode().unwrap()),
            Ok(request)
        );
        let opened = FilesystemStreamOpened { resource_id: 9 };
        assert_eq!(
            FilesystemStreamOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        for event in [
            FilesystemStreamEvent::Metadata {
                offset: 17,
                total_size: 99,
            },
            FilesystemStreamEvent::Chunk {
                offset: 17,
                bytes: vec![1, 2, 3],
            },
            FilesystemStreamEvent::Complete {
                end_offset: 20,
                total_bytes: 3,
                eof: false,
            },
        ] {
            assert_eq!(
                FilesystemStreamEvent::decode(&event.encode().unwrap()),
                Ok(event)
            );
        }
        assert_eq!(
            FilesystemStreamEvent::Chunk {
                offset: 0,
                bytes: vec![0; MAX_FILE_STREAM_CHUNK_BYTES + 1],
            }
            .encode(),
            Err(SchemaError::LimitExceeded)
        );
    }

    #[test]
    fn http_payloads_exclude_arbitrary_headers_and_are_bounded() {
        let request = HttpRequest {
            method: HttpRequestMethod::Post,
            url: "https://api.example.com/v1/state".into(),
            accept: Some("application/json".into()),
            content_type: Some("application/json".into()),
            body: br#"{"enabled":true}"#.to_vec(),
        };
        assert_eq!(HttpRequest::decode(&request.encode().unwrap()), Ok(request));
        let response = HttpResponse {
            status: 200,
            final_url: "https://api.example.com/v1/state".into(),
            content_type: Some("application/json".into()),
            etag: Some("\"revision-1\"".into()),
            body: b"ok".to_vec(),
        };
        assert_eq!(
            HttpResponse::decode(&response.encode().unwrap()),
            Ok(response)
        );
        let too_large = HttpRequest {
            body: vec![0; MAX_HTTP_INLINE_BODY_BYTES as usize + 1],
            ..HttpRequest {
                method: HttpRequestMethod::Post,
                url: "https://api.example.com/".into(),
                accept: None,
                content_type: None,
                body: Vec::new(),
            }
        };
        assert_eq!(too_large.encode(), Err(SchemaError::LimitExceeded));
    }

    #[test]
    fn http_stream_events_are_typed_bounded_and_unambiguous() {
        let opened = HttpStreamOpened { resource_id: 77 };
        assert_eq!(
            HttpStreamOpened::decode(&opened.encode().unwrap()),
            Ok(opened)
        );
        for event in [
            HttpStreamEvent::Metadata {
                status: 200,
                final_url: "https://example.test/v1/data".into(),
                content_type: Some("application/octet-stream".into()),
                etag: Some("v1".into()),
            },
            HttpStreamEvent::Chunk(vec![1, 2, 3]),
            HttpStreamEvent::Complete { total_bytes: 3 },
        ] {
            assert_eq!(HttpStreamEvent::decode(&event.encode().unwrap()), Ok(event));
        }
        assert_eq!(
            HttpStreamEvent::Chunk(vec![0; MAX_HTTP_STREAM_CHUNK_BYTES + 1]).encode(),
            Err(SchemaError::LimitExceeded)
        );
        assert_eq!(
            HttpStreamEvent::Chunk(Vec::new()).encode(),
            Err(SchemaError::LimitExceeded)
        );
    }

    #[test]
    fn command_request_and_stream_events_are_typed_and_bounded() {
        let request = CommandRunRequest {
            command_id: "set-level".into(),
            values: vec![
                CommandValue::Integer {
                    name: "level".into(),
                    value: 42,
                },
                CommandValue::FixedEnum {
                    name: "profile".into(),
                    value: "balanced".into(),
                },
            ],
        };
        assert_eq!(
            CommandRunRequest::decode(&request.encode().unwrap()),
            Ok(request)
        );
        let duplicate = CommandRunRequest {
            command_id: "bad".into(),
            values: vec![
                CommandValue::Text {
                    name: "value".into(),
                    value: "one".into(),
                },
                CommandValue::Text {
                    name: "value".into(),
                    value: "two".into(),
                },
            ],
        };
        assert_eq!(duplicate.encode(), Err(SchemaError::Malformed));
        let opened = CommandOpened { resource_id: 11 };
        assert_eq!(CommandOpened::decode(&opened.encode().unwrap()), Ok(opened));
        for event in [
            CommandEvent::Stdout(b"ok".to_vec()),
            CommandEvent::Stderr(b"warning".to_vec()),
            CommandEvent::Exited {
                exit_code: Some(0),
                signal: None,
                stdout_bytes: 2,
                stderr_bytes: 7,
            },
        ] {
            assert_eq!(CommandEvent::decode(&event.encode().unwrap()), Ok(event));
        }
        assert_eq!(
            CommandEvent::Exited {
                exit_code: None,
                signal: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
            }
            .encode(),
            Err(SchemaError::Malformed)
        );
    }
}
#[test]
fn appearance_publication_is_typed_bounded_and_strict() {
    let publication = AppearancePublish {
        provider: "desktop-theme".into(),
        scheme: AppearanceScheme::Dark,
        background: AppearanceColor {
            red: 1,
            green: 2,
            blue: 3,
        },
        foreground: AppearanceColor {
            red: 4,
            green: 5,
            blue: 6,
        },
        accent: AppearanceColor {
            red: 7,
            green: 8,
            blue: 9,
        },
        selection: AppearanceColor {
            red: 10,
            green: 11,
            blue: 12,
        },
        muted: AppearanceColor {
            red: 13,
            green: 14,
            blue: 15,
        },
        destructive: AppearanceColor {
            red: 16,
            green: 17,
            blue: 18,
        },
    };
    assert_eq!(
        AppearancePublish::decode(&publication.encode().unwrap()),
        Ok(publication.clone())
    );
    let mut trailing = publication.encode().unwrap();
    trailing.push(0);
    assert_eq!(
        AppearancePublish::decode(&trailing),
        Err(SchemaError::Malformed)
    );
    assert_eq!(
        AppearancePublish {
            provider: String::new(),
            ..publication
        }
        .encode(),
        Err(SchemaError::Malformed)
    );
}
