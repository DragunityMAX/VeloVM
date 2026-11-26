// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Binary length-prefixed wire messages between `fastdesk_worker` and `fastdesk-viewer`.

use crate::MAGIC;
use crate::MAX_CLIENT_MSG_PAYLOAD_BYTES;
use crate::MAX_CURSOR_DIM;
use crate::MAX_FRAME_PAYLOAD_BYTES;
use crate::MAX_RECTS_PER_BATCH;
use crate::codec::RectEncoding;
use std::io::Read;
use std::io::Write;
use thiserror::Error;

const MSG_SERVER_HELLO: u8 = 1;
const MSG_RESOLUTION: u8 = 2;
const MSG_FRAME_BATCH: u8 = 3;
const MSG_CURSOR_POSITION: u8 = 4;
const MSG_CURSOR_SHAPE: u8 = 5;
const MSG_PONG: u8 = 6;

const MSG_CLIENT_HELLO: u8 = 101;
const MSG_KEY_EVENT: u8 = 102;
const MSG_POINTER_EVENT: u8 = 103;
const MSG_REQUEST_FULL_FRAME: u8 = 104;
const MSG_PING: u8 = 105;

/// Wire protocol encoding/decoding error.
#[derive(Debug, Error)]
pub enum ProtoError {
    /// Underlying I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Frame was truncated before all expected bytes were read.
    #[error("truncated frame")]
    TruncatedFrame,
    /// Protocol magic mismatch.
    #[error("invalid protocol magic")]
    InvalidMagic,
    /// Unknown message opcode.
    #[error("unknown message type {0}")]
    UnknownMessageType(u8),
    /// Frame payload exceeds [`MAX_FRAME_PAYLOAD_BYTES`].
    #[error("frame payload {0} exceeds maximum")]
    FrameTooLarge(usize),
    /// Optional wire string exceeds `u16::MAX` bytes.
    #[error("string length {0} exceeds maximum")]
    StringTooLarge(usize),
    /// Unknown [`RectEncoding`] tag.
    #[error("invalid rectangle encoding {0}")]
    InvalidEncoding(u8),
    /// Invalid UTF-8 in SHM path string.
    #[error("invalid utf-8 string")]
    InvalidUtf8,
    /// Rectangle or cursor dimension exceeded bounds.
    #[error("invalid dimensions")]
    InvalidDimensions,
    /// Pixel payload size did not match `w * h * 4`.
    #[error("payload length mismatch: expected {expected}, got {actual}")]
    PayloadLengthMismatch {
        /// Expected uncompressed byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
    /// LZ4 decompression failed.
    #[error("lz4 decompression failed: {0}")]
    DecompressFailed(String),
    /// Attempted to pixel-decode a `ShmDirtyOnly` rect.
    #[error("cannot pixel-decode a ShmDirtyOnly rect")]
    UnexpectedShmRect,
    /// Too many rectangles in a single batch.
    #[error("too many rectangles in batch: {0}")]
    TooManyRects(usize),
}

/// An encoded dirty rectangle within a [`ServerMsg::FrameBatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRect {
    /// Left X coordinate in desktop pixels.
    pub x: u16,
    /// Top Y coordinate in desktop pixels.
    pub y: u16,
    /// Width in pixels.
    pub w: u16,
    /// Height in pixels.
    pub h: u16,
    /// Payload encoding.
    pub encoding: RectEncoding,
    /// Encoded payload bytes (empty for [`RectEncoding::ShmDirtyOnly`]).
    pub data: Vec<u8>,
}

/// Server-to-viewer protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// Initial handshake sent immediately upon client connection.
    ServerHello {
        /// Supported capability flags (`CAP_*`).
        capabilities: u32,
        /// Initial desktop width in pixels.
        width: u16,
        /// Initial desktop height in pixels.
        height: u16,
        /// Scanline stride in bytes.
        stride: u32,
        /// Byte offset of the primary surface within VRAM.
        offset: u32,
        /// Total framebuffer VRAM size in bytes.
        vram_size: u32,
        /// Optional backing shared-memory file path for local zero-copy mode.
        ///
        /// Empty strings (`Some("")`) are normalized to `None` on the wire.
        shm_path: Option<String>,
    },
    /// Desktop resolution or format changed.
    Resolution {
        /// New width in pixels.
        width: u16,
        /// New height in pixels.
        height: u16,
        /// New scanline stride in bytes.
        stride: u32,
        /// Byte offset of the primary surface within VRAM.
        offset: u32,
        /// Optional backing shared-memory file path.
        ///
        /// Empty strings (`Some("")`) are normalized to `None` on the wire.
        shm_path: Option<String>,
    },
    /// A batch of dirty rectangles for a single display update.
    FrameBatch {
        /// Monotonic frame sequence number.
        frame_seq: u64,
        /// Host timestamp in microseconds when the batch was emitted.
        host_time_us: u64,
        /// Coalesced dirty rectangles.
        rects: Vec<EncodedRect>,
    },
    /// Hardware pointer visibility and coordinates.
    CursorPosition {
        /// Whether the cursor is visible.
        visible: bool,
        /// Upper-left X position in desktop pixels.
        x: i32,
        /// Upper-left Y position in desktop pixels.
        y: i32,
    },
    /// Hardware pointer shape sprite (`lz4_bgra` compresses `width * height * 4` BGRA bytes).
    CursorShape {
        /// Cursor width in pixels (`1..=96`).
        width: u16,
        /// Cursor height in pixels (`1..=96`).
        height: u16,
        /// X hotspot in pixels.
        hotspot_x: u16,
        /// Y hotspot in pixels.
        hotspot_y: u16,
        /// LZ4-compressed (with prepended uncompressed size) BGRA cursor pixels.
        lz4_bgra: Vec<u8>,
    },
    /// Latency measurement response to [`ClientMsg::Ping`].
    Pong {
        /// Echoed client timestamp in microseconds.
        client_time_us: u64,
        /// Server monotonic timestamp in microseconds.
        host_time_us: u64,
    },
}

/// Viewer-to-server protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    /// Initial handshake sent by the viewer.
    ClientHello {
        /// Viewer capability flags (`CAP_*`).
        capabilities: u32,
        /// Whether the viewer requests zero-copy SHM mode if available locally.
        prefer_shm: bool,
    },
    /// Raw XT Set 1 / `0xE0`-prefixed keyboard scancode event.
    KeyEvent {
        /// XT scancode (`0xE0xx` for extended keys).
        scancode: u16,
        /// `true` on key press (make), `false` on key release (break).
        make: bool,
    },
    /// Absolute HID pointer and scroll wheel event.
    PointerEvent {
        /// Button mask (`BTN_LEFT | BTN_MIDDLE | BTN_RIGHT`).
        button_mask: u8,
        /// Normalized absolute X coordinate (`0..=32767`).
        x: u16,
        /// Normalized absolute Y coordinate (`0..=32767`).
        y: u16,
        /// Relative vertical scroll wheel delta (positive = up, negative = down).
        wheel_delta: i16,
    },
    /// Requests a full framebuffer refresh (e.g. after switching modes or fallback).
    RequestFullFrame {
        /// Whether SHM dirty-only rects are allowed for this refresh.
        allow_shm: bool,
    },
    /// Round-trip latency probe.
    Ping {
        /// Client monotonic timestamp in microseconds.
        client_time_us: u64,
    },
}

/// Decodes a 5-byte frame header `[msg_type: u8][payload_len: u32 LE]`.
pub fn decode_frame_header(header: &[u8; 5]) -> Result<(u8, usize), ProtoError> {
    let msg_type = header[0];
    let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if payload_len > MAX_FRAME_PAYLOAD_BYTES {
        return Err(ProtoError::FrameTooLarge(payload_len));
    }
    Ok((msg_type, payload_len))
}

/// Decodes a 5-byte client-to-server frame header `[msg_type: u8][payload_len: u32 LE]`,
/// enforcing [`MAX_CLIENT_MSG_PAYLOAD_BYTES`].
pub fn decode_client_frame_header(header: &[u8; 5]) -> Result<(u8, usize), ProtoError> {
    let msg_type = header[0];
    let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if payload_len > MAX_CLIENT_MSG_PAYLOAD_BYTES {
        return Err(ProtoError::FrameTooLarge(payload_len));
    }
    Ok((msg_type, payload_len))
}

/// Encodes an optional UTF-8 string as `[len: u16 LE][bytes]`.
///
/// `None` and `Some("")` both encode with length `0` (so `Some("")` is normalized
/// to `None` when decoded). Returns [`ProtoError::StringTooLarge`] if `s` exceeds
/// `u16::MAX` bytes rather than truncating mid-UTF-8.
fn write_opt_str(out: &mut Vec<u8>, s: Option<&str>) -> Result<(), ProtoError> {
    match s {
        Some(val) if !val.is_empty() => {
            let bytes = val.as_bytes();
            let len = u16::try_from(bytes.len())
                .map_err(|_| ProtoError::StringTooLarge(bytes.len()))?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Some(_) | None => out.extend_from_slice(&0u16.to_le_bytes()),
    }
    Ok(())
}

struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.buf.len() < n {
            return Err(ProtoError::TruncatedFrame);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtoError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtoError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, ProtoError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i16(&mut self) -> Result<i16, ProtoError> {
        let b = self.take(2)?;
        Ok(i16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64, ProtoError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Decodes an optional UTF-8 string encoded as `[len: u16 LE][bytes]`.
    /// Length `0` maps to `None` (normalizing any encoded `Some("")`).
    fn opt_str(&mut self) -> Result<Option<String>, ProtoError> {
        let len = self.u16()? as usize;
        if len == 0 {
            return Ok(None);
        }
        let bytes = self.take(len)?;
        let s = std::str::from_utf8(bytes).map_err(|_| ProtoError::InvalidUtf8)?;
        Ok(Some(s.to_owned()))
    }
}

fn begin_frame(out: &mut Vec<u8>, msg_type: u8) -> usize {
    out.push(msg_type);
    let len_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    len_pos
}

fn finish_frame(out: &mut Vec<u8>, len_pos: usize) -> Result<(), ProtoError> {
    let payload_len = out.len() - (len_pos + 4);
    if payload_len > MAX_FRAME_PAYLOAD_BYTES {
        out.truncate(len_pos - 1);
        return Err(ProtoError::FrameTooLarge(payload_len));
    }
    out[len_pos..len_pos + 4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    Ok(())
}

/// Appends a framed [`ServerMsg`] (`[type: u8][len: u32 LE][payload]`) to `out`.
pub fn encode_server_msg(msg: &ServerMsg, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    match msg {
        ServerMsg::ServerHello {
            capabilities,
            width,
            height,
            stride,
            offset,
            vram_size,
            shm_path,
        } => {
            let len_pos = begin_frame(out, MSG_SERVER_HELLO);
            out.extend_from_slice(&MAGIC);
            out.extend_from_slice(&capabilities.to_le_bytes());
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&stride.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&vram_size.to_le_bytes());
            if let Err(err) = write_opt_str(out, shm_path.as_deref()) {
                out.truncate(len_pos - 1);
                return Err(err);
            }
            finish_frame(out, len_pos)
        }
        ServerMsg::Resolution {
            width,
            height,
            stride,
            offset,
            shm_path,
        } => {
            let len_pos = begin_frame(out, MSG_RESOLUTION);
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&stride.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            if let Err(err) = write_opt_str(out, shm_path.as_deref()) {
                out.truncate(len_pos - 1);
                return Err(err);
            }
            finish_frame(out, len_pos)
        }
        ServerMsg::FrameBatch {
            frame_seq,
            host_time_us,
            rects,
        } => {
            if rects.len() > MAX_RECTS_PER_BATCH {
                return Err(ProtoError::TooManyRects(rects.len()));
            }
            let len_pos = begin_frame(out, MSG_FRAME_BATCH);
            out.extend_from_slice(&frame_seq.to_le_bytes());
            out.extend_from_slice(&host_time_us.to_le_bytes());
            out.extend_from_slice(&(rects.len() as u16).to_le_bytes());
            for rect in rects {
                out.extend_from_slice(&rect.x.to_le_bytes());
                out.extend_from_slice(&rect.y.to_le_bytes());
                out.extend_from_slice(&rect.w.to_le_bytes());
                out.extend_from_slice(&rect.h.to_le_bytes());
                out.push(rect.encoding as u8);
                out.extend_from_slice(&(rect.data.len() as u32).to_le_bytes());
                out.extend_from_slice(&rect.data);
            }
            finish_frame(out, len_pos)
        }
        ServerMsg::CursorPosition { visible, x, y } => {
            let len_pos = begin_frame(out, MSG_CURSOR_POSITION);
            out.push(u8::from(*visible));
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            finish_frame(out, len_pos)
        }
        ServerMsg::CursorShape {
            width,
            height,
            hotspot_x,
            hotspot_y,
            lz4_bgra,
        } => {
            let len_pos = begin_frame(out, MSG_CURSOR_SHAPE);
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&hotspot_x.to_le_bytes());
            out.extend_from_slice(&hotspot_y.to_le_bytes());
            out.extend_from_slice(&(lz4_bgra.len() as u32).to_le_bytes());
            out.extend_from_slice(lz4_bgra);
            finish_frame(out, len_pos)
        }
        ServerMsg::Pong {
            client_time_us,
            host_time_us,
        } => {
            let len_pos = begin_frame(out, MSG_PONG);
            out.extend_from_slice(&client_time_us.to_le_bytes());
            out.extend_from_slice(&host_time_us.to_le_bytes());
            finish_frame(out, len_pos)
        }
    }
}

/// Decodes a [`ServerMsg`] from its frame opcode and payload bytes.
pub fn decode_server_msg(msg_type: u8, payload: &[u8]) -> Result<ServerMsg, ProtoError> {
    let mut c = Cursor::new(payload);
    match msg_type {
        MSG_SERVER_HELLO => {
            let magic = c.take(MAGIC.len())?;
            if magic != MAGIC {
                return Err(ProtoError::InvalidMagic);
            }
            Ok(ServerMsg::ServerHello {
                capabilities: c.u32()?,
                width: c.u16()?,
                height: c.u16()?,
                stride: c.u32()?,
                offset: c.u32()?,
                vram_size: c.u32()?,
                shm_path: c.opt_str()?,
            })
        }
        MSG_RESOLUTION => Ok(ServerMsg::Resolution {
            width: c.u16()?,
            height: c.u16()?,
            stride: c.u32()?,
            offset: c.u32()?,
            shm_path: c.opt_str()?,
        }),
        MSG_FRAME_BATCH => {
            let frame_seq = c.u64()?;
            let host_time_us = c.u64()?;
            let count = c.u16()? as usize;
            if count > MAX_RECTS_PER_BATCH {
                return Err(ProtoError::TooManyRects(count));
            }
            let mut rects = Vec::with_capacity(count);
            for _ in 0..count {
                let x = c.u16()?;
                let y = c.u16()?;
                let w = c.u16()?;
                let h = c.u16()?;
                let encoding = RectEncoding::try_from(c.u8()?)?;
                let data_len = c.u32()? as usize;
                let data = c.take(data_len)?.to_vec();
                rects.push(EncodedRect {
                    x,
                    y,
                    w,
                    h,
                    encoding,
                    data,
                });
            }
            Ok(ServerMsg::FrameBatch {
                frame_seq,
                host_time_us,
                rects,
            })
        }
        MSG_CURSOR_POSITION => Ok(ServerMsg::CursorPosition {
            visible: c.u8()? != 0,
            x: c.i32()?,
            y: c.i32()?,
        }),
        MSG_CURSOR_SHAPE => {
            let width = c.u16()?;
            let height = c.u16()?;
            if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
                return Err(ProtoError::InvalidDimensions);
            }
            let hotspot_x = c.u16()?;
            let hotspot_y = c.u16()?;
            let data_len = c.u32()? as usize;
            let lz4_bgra = c.take(data_len)?.to_vec();
            Ok(ServerMsg::CursorShape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                lz4_bgra,
            })
        }
        MSG_PONG => Ok(ServerMsg::Pong {
            client_time_us: c.u64()?,
            host_time_us: c.u64()?,
        }),
        other => Err(ProtoError::UnknownMessageType(other)),
    }
}

/// Appends a framed [`ClientMsg`] (`[type: u8][len: u32 LE][payload]`) to `out`.
pub fn encode_client_msg(msg: &ClientMsg, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    match msg {
        ClientMsg::ClientHello {
            capabilities,
            prefer_shm,
        } => {
            let len_pos = begin_frame(out, MSG_CLIENT_HELLO);
            out.extend_from_slice(&MAGIC);
            out.extend_from_slice(&capabilities.to_le_bytes());
            out.push(u8::from(*prefer_shm));
            finish_frame(out, len_pos)
        }
        ClientMsg::KeyEvent { scancode, make } => {
            let len_pos = begin_frame(out, MSG_KEY_EVENT);
            out.extend_from_slice(&scancode.to_le_bytes());
            out.push(u8::from(*make));
            finish_frame(out, len_pos)
        }
        ClientMsg::PointerEvent {
            button_mask,
            x,
            y,
            wheel_delta,
        } => {
            let len_pos = begin_frame(out, MSG_POINTER_EVENT);
            out.push(*button_mask);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&wheel_delta.to_le_bytes());
            finish_frame(out, len_pos)
        }
        ClientMsg::RequestFullFrame { allow_shm } => {
            let len_pos = begin_frame(out, MSG_REQUEST_FULL_FRAME);
            out.push(u8::from(*allow_shm));
            finish_frame(out, len_pos)
        }
        ClientMsg::Ping { client_time_us } => {
            let len_pos = begin_frame(out, MSG_PING);
            out.extend_from_slice(&client_time_us.to_le_bytes());
            finish_frame(out, len_pos)
        }
    }
}

/// Decodes a [`ClientMsg`] from its frame opcode and payload bytes.
pub fn decode_client_msg(msg_type: u8, payload: &[u8]) -> Result<ClientMsg, ProtoError> {
    let mut c = Cursor::new(payload);
    match msg_type {
        MSG_CLIENT_HELLO => {
            let magic = c.take(MAGIC.len())?;
            if magic != MAGIC {
                return Err(ProtoError::InvalidMagic);
            }
            Ok(ClientMsg::ClientHello {
                capabilities: c.u32()?,
                prefer_shm: c.u8()? != 0,
            })
        }
        MSG_KEY_EVENT => Ok(ClientMsg::KeyEvent {
            scancode: c.u16()?,
            make: c.u8()? != 0,
        }),
        MSG_POINTER_EVENT => Ok(ClientMsg::PointerEvent {
            button_mask: c.u8()?,
            x: c.u16()?,
            y: c.u16()?,
            wheel_delta: c.i16()?,
        }),
        MSG_REQUEST_FULL_FRAME => Ok(ClientMsg::RequestFullFrame {
            allow_shm: c.u8()? != 0,
        }),
        MSG_PING => Ok(ClientMsg::Ping {
            client_time_us: c.u64()?,
        }),
        other => Err(ProtoError::UnknownMessageType(other)),
    }
}

/// Writes one framed [`ServerMsg`] to a synchronous stream.
pub fn write_server_msg(writer: &mut impl Write, msg: &ServerMsg) -> Result<(), ProtoError> {
    let mut buf = Vec::new();
    encode_server_msg(msg, &mut buf)?;
    writer.write_all(&buf)?;
    Ok(())
}

/// Reads one framed [`ServerMsg`] from a synchronous stream.
pub fn read_server_msg(reader: &mut impl Read) -> Result<ServerMsg, ProtoError> {
    let mut hdr = [0u8; 5];
    reader.read_exact(&mut hdr)?;
    let (msg_type, payload_len) = decode_frame_header(&hdr)?;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    decode_server_msg(msg_type, &payload)
}

/// Writes one framed [`ClientMsg`] to a synchronous stream.
pub fn write_client_msg(writer: &mut impl Write, msg: &ClientMsg) -> Result<(), ProtoError> {
    let mut buf = Vec::new();
    encode_client_msg(msg, &mut buf)?;
    writer.write_all(&buf)?;
    Ok(())
}

/// Reads one framed [`ClientMsg`] from a synchronous stream.
pub fn read_client_msg(reader: &mut impl Read) -> Result<ClientMsg, ProtoError> {
    let mut hdr = [0u8; 5];
    reader.read_exact(&mut hdr)?;
    let (msg_type, payload_len) = decode_client_frame_header(&hdr)?;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    decode_client_msg(msg_type, &payload)
}
