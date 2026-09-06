use std::{collections::BTreeSet, error::Error, fmt};

use crate::MAX_ASSETS;

const MAGIC: &[u8; 8] = b"OTBAS001";
pub const MAX_ASSET_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_ASSET_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ASSET_ID_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundledAsset {
    pub id: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetBundleError(String);

impl AssetBundleError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AssetBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AssetBundleError {}

pub fn encode_asset_bundle<'a>(
    assets: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<Vec<u8>, AssetBundleError> {
    let assets = assets.into_iter().collect::<Vec<_>>();
    if assets.len() > MAX_ASSETS {
        return Err(AssetBundleError::new(
            "asset bundle contains too many assets",
        ));
    }
    let mut output = Vec::with_capacity(12);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&(assets.len() as u32).to_le_bytes());
    let mut ids = BTreeSet::new();
    for (id, bytes) in assets {
        if id.is_empty() || id.len() > MAX_ASSET_ID_BYTES {
            return Err(AssetBundleError::new("asset bundle ID is not bounded"));
        }
        if !ids.insert(id) {
            return Err(AssetBundleError::new(
                "asset bundle contains a duplicate ID",
            ));
        }
        if bytes.len() > MAX_ASSET_FILE_BYTES {
            return Err(AssetBundleError::new(
                "encoded asset exceeds its size limit",
            ));
        }
        let id_length = u16::try_from(id.len())
            .map_err(|_| AssetBundleError::new("asset bundle ID length overflow"))?;
        let byte_length = u32::try_from(bytes.len())
            .map_err(|_| AssetBundleError::new("encoded asset length overflow"))?;
        output.extend_from_slice(&id_length.to_le_bytes());
        output.extend_from_slice(id.as_bytes());
        output.extend_from_slice(&byte_length.to_le_bytes());
        output.extend_from_slice(bytes);
        if output.len() > MAX_ASSET_BUNDLE_BYTES {
            return Err(AssetBundleError::new("asset bundle exceeds its size limit"));
        }
    }
    Ok(output)
}

pub fn decode_asset_bundle(bytes: &[u8]) -> Result<Vec<BundledAsset>, AssetBundleError> {
    if bytes.len() > MAX_ASSET_BUNDLE_BYTES {
        return Err(AssetBundleError::new("asset bundle exceeds its size limit"));
    }
    let mut input = Input { bytes, offset: 0 };
    if input.take(MAGIC.len())? != MAGIC {
        return Err(AssetBundleError::new("asset bundle magic is invalid"));
    }
    let count = input.u32()? as usize;
    if count > MAX_ASSETS {
        return Err(AssetBundleError::new(
            "asset bundle contains too many assets",
        ));
    }
    let mut output = Vec::with_capacity(count);
    let mut ids = BTreeSet::new();
    for _ in 0..count {
        let id_length = input.u16()? as usize;
        if id_length == 0 || id_length > MAX_ASSET_ID_BYTES {
            return Err(AssetBundleError::new("asset bundle ID is not bounded"));
        }
        let id = std::str::from_utf8(input.take(id_length)?)
            .map_err(|_| AssetBundleError::new("asset bundle ID is not UTF-8"))?
            .to_owned();
        if !ids.insert(id.clone()) {
            return Err(AssetBundleError::new(
                "asset bundle contains a duplicate ID",
            ));
        }
        let byte_length = input.u32()? as usize;
        if byte_length > MAX_ASSET_FILE_BYTES {
            return Err(AssetBundleError::new(
                "encoded asset exceeds its size limit",
            ));
        }
        output.push(BundledAsset {
            id,
            bytes: input.take(byte_length)?.to_vec(),
        });
    }
    if input.offset != bytes.len() {
        return Err(AssetBundleError::new("asset bundle has trailing bytes"));
    }
    Ok(output)
}

struct Input<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], AssetBundleError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| AssetBundleError::new("asset bundle length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| AssetBundleError::new("asset bundle is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, AssetBundleError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, AssetBundleError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_ordered_and_exact() {
        let encoded =
            encode_asset_bundle([("mark", b"svg".as_slice()), ("art", b"png".as_slice())]).unwrap();
        assert_eq!(
            decode_asset_bundle(&encoded).unwrap(),
            [
                BundledAsset {
                    id: "mark".into(),
                    bytes: b"svg".to_vec(),
                },
                BundledAsset {
                    id: "art".into(),
                    bytes: b"png".to_vec(),
                }
            ]
        );
    }

    #[test]
    fn rejects_truncation_duplicates_and_trailing_data() {
        let encoded = encode_asset_bundle([("mark", b"svg".as_slice())]).unwrap();
        assert!(decode_asset_bundle(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode_asset_bundle(&trailing).is_err());
        assert!(encode_asset_bundle([("mark", &[][..]), ("mark", &[][..])]).is_err());
    }
}
