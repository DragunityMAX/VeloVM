// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire protocol, 16x16 dirty-tile coalescing, and lossless LZ4 codec for
//! OpenVMM FastDesk (`--fastdesk`) and `fastdesk-viewer`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod dirty;
pub mod wire;

pub use codec::RectEncoding;
pub use codec::decode_bgra_rect;
pub use codec::encode_bgra_rect;
pub use dirty::DirtyTileTracker;
pub use dirty::TILE_SIZE;
pub use dirty::TileRect;
pub use wire::ClientMsg;
pub use wire::EncodedRect;
pub use wire::ProtoError;
pub use wire::ServerMsg;
pub use wire::decode_client_frame_header;
pub use wire::decode_client_msg;
pub use wire::decode_frame_header;
pub use wire::decode_server_msg;
pub use wire::encode_client_msg;
pub use wire::encode_server_msg;
pub use wire::read_client_msg;
pub use wire::read_server_msg;
pub use wire::write_client_msg;
pub use wire::write_server_msg;

/// Protocol magic banner sent in [`ServerMsg::ServerHello`] and [`ClientMsg::ClientHello`].
pub const MAGIC: [u8; 6] = *b"FDSK02";

/// Default TCP port for the FastDesk server.
pub const DEFAULT_TCP_PORT: u16 = 5901;

/// Default AF_VSOCK / AF_HYPERV port for the FastDesk server.
pub const DEFAULT_VSOCK_PORT: u32 = 5901;

/// Capability flag: lossless LZ4 compressed dirty rectangles.
pub const CAP_LZ4_TILES: u32 = 1 << 0;

/// Capability flag: zero-copy shared memory (`ShmDirtyOnly`) updates.
pub const CAP_SHM_ZEROCOPY: u32 = 1 << 1;

/// Capability flag: out-of-band hardware cursor shape and position updates.
pub const CAP_HW_CURSOR: u32 = 1 << 2;

/// Bitmask of all capabilities supported by this protocol version.
pub const CAP_ALL: u32 = CAP_LZ4_TILES | CAP_SHM_ZEROCOPY | CAP_HW_CURSOR;

/// Mouse button bit: primary (left) button, matching `uidevices::mouse`.
pub const BTN_LEFT: u8 = 0x01;

/// Mouse button bit: middle button, matching `uidevices::mouse`.
pub const BTN_MIDDLE: u8 = 0x02;

/// Mouse button bit: secondary (right) button, matching `uidevices::mouse`.
pub const BTN_RIGHT: u8 = 0x04;

/// Mouse button bit: scroll wheel up pulse, matching `uidevices::mouse`.
pub const BTN_WHEEL_UP: u8 = 0x08;

/// Mouse button bit: scroll wheel down pulse, matching `uidevices::mouse`.
pub const BTN_WHEEL_DOWN: u8 = 0x10;

/// Maximum dirty rectangles allowed in a single [`ServerMsg::FrameBatch`].
pub const MAX_RECTS_PER_BATCH: usize = 4096;

/// Maximum wire frame payload size (64 MiB).
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Maximum wire payload size for client-to-server control/input messages (1 KiB).
pub const MAX_CLIENT_MSG_PAYLOAD_BYTES: usize = 1024;

/// Maximum cursor width or height in pixels (SynthVid limit).
pub const MAX_CURSOR_DIM: u16 = 96;

/// Formats the Hyper-V `AF_HYPERV` service GUID corresponding to a vsock port.
pub fn vsock_service_guid_string(port: u32) -> String {
    format!("{port:08x}-facb-11e6-bd58-64006a7986d3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip_server(msg: &ServerMsg) -> ServerMsg {
        let mut wire = Vec::new();
        write_server_msg(&mut wire, msg).unwrap();
        let mut cursor = Cursor::new(wire);
        read_server_msg(&mut cursor).unwrap()
    }

    fn roundtrip_client(msg: &ClientMsg) -> ClientMsg {
        let mut wire = Vec::new();
        write_client_msg(&mut wire, msg).unwrap();
        let mut cursor = Cursor::new(wire);
        read_client_msg(&mut cursor).unwrap()
    }

    #[test]
    fn wire_roundtrips_all_messages() {
        let server_msgs = vec![
            ServerMsg::ServerHello {
                capabilities: CAP_ALL,
                width: 1920,
                height: 1080,
                stride: 1920 * 4,
                offset: 4096,
                vram_size: 32 * 1024 * 1024,
                shm_path: Some("/tmp/vram.shm".into()),
            },
            ServerMsg::Resolution {
                width: 3840,
                height: 2160,
                stride: 3840 * 4,
                offset: 8192,
                shm_path: None,
            },
            ServerMsg::FrameBatch {
                frame_seq: 42,
                host_time_us: 123_456_789,
                rects: vec![
                    EncodedRect {
                        x: 16,
                        y: 32,
                        w: 64,
                        h: 48,
                        encoding: RectEncoding::Lz4Bgra,
                        data: vec![1, 2, 3, 4],
                    },
                    EncodedRect {
                        x: 100,
                        y: 200,
                        w: 32,
                        h: 32,
                        encoding: RectEncoding::ShmDirtyOnly,
                        data: Vec::new(),
                    },
                ],
            },
            ServerMsg::CursorPosition {
                visible: true,
                x: -4,
                y: 512,
            },
            ServerMsg::CursorShape {
                width: 24,
                height: 24,
                hotspot_x: 3,
                hotspot_y: 5,
                lz4_bgra: vec![9, 8, 7, 6],
            },
            ServerMsg::Pong {
                client_time_us: 1000,
                host_time_us: 2000,
            },
        ];

        for msg in &server_msgs {
            assert_eq!(&roundtrip_server(msg), msg);
        }

        let client_msgs = vec![
            ClientMsg::ClientHello {
                capabilities: CAP_ALL,
                prefer_shm: true,
            },
            ClientMsg::KeyEvent {
                scancode: 0xe048,
                make: true,
            },
            ClientMsg::PointerEvent {
                button_mask: BTN_LEFT | BTN_RIGHT,
                x: 16384,
                y: 8192,
                wheel_delta: -3,
            },
            ClientMsg::RequestFullFrame { allow_shm: false },
            ClientMsg::Ping {
                client_time_us: 98765,
            },
        ];

        for msg in &client_msgs {
            assert_eq!(&roundtrip_client(msg), msg);
        }
    }

    #[test]
    fn lz4_codec_and_raw_fallback() {
        // Compressible desktop solid/gradient region.
        let mut raw = Vec::with_capacity(64 * 64 * 4);
        for y in 0..64u8 {
            for x in 0..64u8 {
                raw.extend_from_slice(&[x & 0xf0, y & 0xf0, 0x80, 0xff]);
            }
        }
        let (enc, payload) = encode_bgra_rect(&raw);
        assert_eq!(enc, RectEncoding::Lz4Bgra);
        assert!(payload.len() < raw.len());
        let decoded = decode_bgra_rect(enc, &payload, raw.len()).unwrap();
        assert_eq!(decoded, raw);

        // Tiny < 64-byte payload stays RawBgra.
        let tiny = vec![0xaa; 32];
        let (enc_tiny, payload_tiny) = encode_bgra_rect(&tiny);
        assert_eq!(enc_tiny, RectEncoding::RawBgra);
        assert_eq!(decode_bgra_rect(enc_tiny, &payload_tiny, 32).unwrap(), tiny);

        // Mismatched expected length must fail cleanly.
        let err = decode_bgra_rect(enc, &payload, raw.len() + 4).unwrap_err();
        assert!(matches!(err, ProtoError::PayloadLengthMismatch { .. }));

        // Decompression bomb / tampered prepended size must fail cleanly.
        let mut tampered = payload.clone();
        tampered[..4].copy_from_slice(&(128u32 * 1024 * 1024).to_le_bytes());
        assert!(decode_bgra_rect(RectEncoding::Lz4Bgra, &tampered, raw.len()).is_err());
    }

    #[test]
    fn dirty_tile_tracker_merge_and_diff() {
        let mut tracker = DirtyTileTracker::new(100, 50);
        tracker.clear();
        assert!(tracker.is_empty());

        // Mark a 32x32 region covering tiles (1..=2, 1..=2) and verify single coalesced rect.
        tracker.mark_rect(16, 16, 48, 48);
        // Out-of-bounds clamping test.
        tracker.mark_rect(90, 40, 200, 200);
        let mut merged = Vec::new();
        tracker.merge_into(&mut merged);
        assert_eq!(
            merged,
            vec![
                TileRect {
                    x: 16,
                    y: 16,
                    w: 32,
                    h: 32,
                },
                TileRect {
                    x: 80,
                    y: 32,
                    w: 20,
                    h: 18,
                },
            ]
        );

        // Frame differencing fallback.
        let stride = 64 * 4;
        let prev = vec![0u8; 64 * stride];
        let mut cur = prev.clone();
        // Touch pixel at (20, 22) -> tile (1, 1) -> [16..32, 16..32].
        cur[22 * stride + 20 * 4] = 0xff;
        let mut diff_tracker = DirtyTileTracker::new(64, 64);
        diff_tracker.clear();
        diff_tracker.diff_frames(&prev, &cur, stride);
        diff_tracker.merge_into(&mut merged);
        assert_eq!(
            merged,
            vec![TileRect {
                x: 16,
                y: 16,
                w: 16,
                h: 16,
            }]
        );
    }

    #[test]
    fn decode_client_frame_header_rejects_oversized_payload() {
        let mut hdr = [101u8, 0, 0, 0, 0];
        hdr[1..5].copy_from_slice(&(MAX_CLIENT_MSG_PAYLOAD_BYTES as u32).to_le_bytes());
        assert_eq!(
            decode_client_frame_header(&hdr).unwrap(),
            (101, MAX_CLIENT_MSG_PAYLOAD_BYTES)
        );

        hdr[1..5].copy_from_slice(&((MAX_CLIENT_MSG_PAYLOAD_BYTES as u32) + 1).to_le_bytes());
        assert!(matches!(
            decode_client_frame_header(&hdr),
            Err(ProtoError::FrameTooLarge(n)) if n == MAX_CLIENT_MSG_PAYLOAD_BYTES + 1
        ));

        hdr[1..5].copy_from_slice(&(64u32 * 1024 * 1024).to_le_bytes());
        assert!(matches!(
            decode_client_frame_header(&hdr),
            Err(ProtoError::FrameTooLarge(n)) if n == 64 * 1024 * 1024
        ));
    }

    #[test]
    fn encode_server_msg_rolls_back_partial_frame_on_frame_too_large() {
        let mut out = Vec::new();
        let valid = ServerMsg::Pong {
            client_time_us: 42,
            host_time_us: 99,
        };
        encode_server_msg(&valid, &mut out).unwrap();
        let valid_len = out.len();

        let oversized = ServerMsg::CursorShape {
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            lz4_bgra: vec![0u8; MAX_FRAME_PAYLOAD_BYTES],
        };
        let err = encode_server_msg(&oversized, &mut out).unwrap_err();
        assert!(matches!(err, ProtoError::FrameTooLarge(n) if n > MAX_FRAME_PAYLOAD_BYTES));
        assert_eq!(
            out.len(),
            valid_len,
            "failed encode must roll back partial frame bytes"
        );

        let mut cursor = std::io::Cursor::new(&out);
        assert_eq!(read_server_msg(&mut cursor).unwrap(), valid);
        assert_eq!(cursor.position() as usize, valid_len);
    }

    #[test]
    fn diff_frames_marks_all_dirty_without_panic_on_stride_overflow() {
        let mut tracker = DirtyTileTracker::new(16, 16);
        tracker.clear();
        assert!(tracker.is_empty());

        let buf = [0u8; 64];
        // On 64-bit targets (16 - 1) * (usize::MAX / 2) overflows usize;
        // fallback must mark all tiles dirty instead of panicking or wrapping.
        tracker.diff_frames(&buf, &buf, usize::MAX / 2);
        let mut merged = Vec::new();
        tracker.merge_into(&mut merged);
        assert_eq!(
            merged,
            vec![TileRect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            }]
        );
    }
}
