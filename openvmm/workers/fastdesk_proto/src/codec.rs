// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lossless LZ4 rectangle encoding with automatic raw fallback.

use crate::wire::ProtoError;

/// Pixel payload encoding for a dirty rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RectEncoding {
    /// Uncompressed tightly-packed 32-bpp BGRA bytes (`w * h * 4`).
    RawBgra = 0,
    /// LZ4 block-compressed BGRA bytes with 4-byte little-endian size prefix.
    Lz4Bgra = 1,
    /// Zero-copy shared memory notification; pixel payload is empty (`0` bytes)
    /// and the viewer reads directly from mapped guest VRAM.
    ShmDirtyOnly = 2,
}

impl TryFrom<u8> for RectEncoding {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::RawBgra),
            1 => Ok(Self::Lz4Bgra),
            2 => Ok(Self::ShmDirtyOnly),
            other => Err(ProtoError::InvalidEncoding(other)),
        }
    }
}

/// Compresses `raw_bgra` using LZ4 when beneficial, falling back to [`RectEncoding::RawBgra`]
/// for tiny or incompressible payloads.
pub fn encode_bgra_rect(raw_bgra: &[u8]) -> (RectEncoding, Vec<u8>) {
    if raw_bgra.len() < 64 {
        return (RectEncoding::RawBgra, raw_bgra.to_vec());
    }
    let compressed = lz4_flex::compress_prepend_size(raw_bgra);
    if compressed.len() < raw_bgra.len() {
        (RectEncoding::Lz4Bgra, compressed)
    } else {
        (RectEncoding::RawBgra, raw_bgra.to_vec())
    }
}

/// Decodes a rectangle payload into `expected_len` raw BGRA bytes.
pub fn decode_bgra_rect(
    encoding: RectEncoding,
    data: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, ProtoError> {
    match encoding {
        RectEncoding::RawBgra => {
            if data.len() != expected_len {
                return Err(ProtoError::PayloadLengthMismatch {
                    expected: expected_len,
                    actual: data.len(),
                });
            }
            Ok(data.to_vec())
        }
        RectEncoding::Lz4Bgra => {
            if data.len() < 4 {
                return Err(ProtoError::TruncatedFrame);
            }
            let declared = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if declared != expected_len {
                return Err(ProtoError::PayloadLengthMismatch {
                    expected: expected_len,
                    actual: declared,
                });
            }
            let out = lz4_flex::decompress_size_prepended(data)
                .map_err(|err| ProtoError::DecompressFailed(err.to_string()))?;
            if out.len() != expected_len {
                return Err(ProtoError::PayloadLengthMismatch {
                    expected: expected_len,
                    actual: out.len(),
                });
            }
            Ok(out)
        }
        RectEncoding::ShmDirtyOnly => Err(ProtoError::UnexpectedShmRect),
    }
}
