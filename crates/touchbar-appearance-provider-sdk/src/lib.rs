//! Guest bindings for sandboxed appearance-provider workers.

#![allow(clippy::too_many_arguments)] // generated canonical-ABI lowering

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "appearance-provider",
        pub_export_macro: true,
        default_bindings_module: "touchbar_appearance_provider_sdk::bindings",
    });
}

pub use bindings::*;
pub use touchbar_broker_schema::{
    AppearanceColor, AppearancePublish, AppearanceScheme, FilesystemFileChunk, FilesystemReadFile,
    SchemaError,
};

use bindings::touchbar::plugin::broker;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

pub fn read_file(request_value: &FilesystemReadFile) -> Result<RequestId, SubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| SubmitError::InvalidPayload)?;
    broker::request("appearance.provide.v1", "read-file", &payload)
        .map(RequestId)
        .map_err(SubmitError::Broker)
}

pub fn publish(publication: &AppearancePublish) -> Result<RequestId, SubmitError> {
    let payload = publication
        .encode()
        .map_err(|_| SubmitError::InvalidPayload)?;
    broker::request("appearance.provide.v1", "publish", &payload)
        .map(RequestId)
        .map_err(SubmitError::Broker)
}

pub fn decode_file_chunk(payload: &[u8]) -> Result<FilesystemFileChunk, SchemaError> {
    FilesystemFileChunk::decode(payload)
}
