use fastdesk_proto::EncodedRect;
use fastdesk_proto::RectEncoding;
use fastdesk_proto::decode_bgra_rect;
use memmap2::Mmap;
use std::fs::File;

#[derive(Debug, Default)]
struct CursorState {
    visible: bool,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
    bgra: Vec<u8>,
    prev_drawn: Option<(i32, i32, u16, u16)>,
}

pub struct DesktopCompositor {
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    pub offset: u32,
    pub backing_fb: Vec<u32>,
    pub present_fb: Vec<u32>,
    shm_mmap: Option<Mmap>,
    cursor: CursorState,
}

impl DesktopCompositor {
    pub fn new(
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        shm_path: Option<&str>,
        allow_shm: bool,
    ) -> anyhow::Result<Self> {
        if width == 0 || height == 0 || (stride as usize) < (width as usize * 4) {
            anyhow::bail!(
                "invalid compositor dimensions: {}x{} stride {}",
                width,
                height,
                stride
            );
        }
        let len = width as usize * height as usize;
        let mut comp = Self {
            width,
            height,
            stride,
            offset,
            backing_fb: vec![0; len],
            present_fb: vec![0; len],
            shm_mmap: None,
            cursor: CursorState::default(),
        };
        if allow_shm {
            if let Some(path) = shm_path {
                comp.try_open_shm(path);
            }
        }
        Ok(comp)
    }

    pub fn is_shm_active(&self) -> bool {
        self.shm_mmap.is_some()
    }

    pub fn try_open_shm(&mut self, path: &str) {
        self.shm_mmap = None;
        let required_end = if self.width > 0 && self.height > 0 {
            let Some(end) = (self.height as usize - 1)
                .checked_mul(self.stride as usize)
                .and_then(|rows| (self.offset as usize).checked_add(rows))
                .and_then(|o| o.checked_add(self.width as usize * 4))
            else {
                return;
            };
            end
        } else {
            self.offset as usize
        };
        if let Ok(file) = File::open(path) {
            // SAFETY: file is opened read-only and maps shared guest VRAM maintained by OpenVMM.
            if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                if mmap.len() >= required_end {
                    self.shm_mmap = Some(mmap);
                }
            }
        }
    }

    pub fn resize(
        &mut self,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        shm_path: Option<&str>,
        allow_shm: bool,
    ) {
        self.width = width;
        self.height = height;
        self.stride = stride;
        self.offset = offset;
        let len = width as usize * height as usize;
        self.backing_fb.resize(len, 0);
        self.backing_fb.fill(0);
        self.present_fb.resize(len, 0);
        self.present_fb.fill(0);
        self.cursor.prev_drawn = None;
        if allow_shm {
            if let Some(path) = shm_path {
                self.try_open_shm(path);
            } else {
                self.shm_mmap = None;
            }
        } else {
            self.shm_mmap = None;
        }
    }

    pub fn set_cursor_position(&mut self, visible: bool, x: i32, y: i32) {
        self.cursor.visible = visible;
        self.cursor.x = x;
        self.cursor.y = y;
    }

    pub fn set_cursor_shape(
        &mut self,
        width: u16,
        height: u16,
        _hotspot_x: u16,
        _hotspot_y: u16,
        lz4_bgra: &[u8],
    ) -> anyhow::Result<()> {
        if width == 0
            || height == 0
            || width > fastdesk_proto::MAX_CURSOR_DIM
            || height > fastdesk_proto::MAX_CURSOR_DIM
        {
            anyhow::bail!("invalid cursor dimensions: {width}x{height}");
        }
        let expected = width as usize * height as usize * 4;
        if lz4_bgra.len() < 4 {
            anyhow::bail!("truncated cursor payload: {} bytes", lz4_bgra.len());
        }
        let declared =
            u32::from_le_bytes([lz4_bgra[0], lz4_bgra[1], lz4_bgra[2], lz4_bgra[3]]) as usize;
        if declared != expected {
            anyhow::bail!("cursor size mismatch: expected {expected}, got {declared}");
        }
        let decoded = decode_bgra_rect(RectEncoding::Lz4Bgra, lz4_bgra, expected)
            .map_err(|e| anyhow::anyhow!("cursor decompress error: {e}"))?;
        self.cursor.width = width;
        self.cursor.height = height;
        self.cursor.bgra = decoded;
        Ok(())
    }

    fn restore_cursor_backing(&mut self) {
        let Some((cx, cy, cw, ch)) = self.cursor.prev_drawn.take() else {
            return;
        };
        let fb_w = self.width as i32;
        let fb_h = self.height as i32;
        let x0 = cx.max(0);
        let y0 = cy.max(0);
        let x1 = (cx + cw as i32).min(fb_w);
        let y1 = (cy + ch as i32).min(fb_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let span = (x1 - x0) as usize;
        let stride = self.width as usize;
        for y in y0..y1 {
            let row_off = y as usize * stride + x0 as usize;
            self.present_fb[row_off..row_off + span]
                .copy_from_slice(&self.backing_fb[row_off..row_off + span]);
        }
    }

    pub fn apply_rects(&mut self, rects: &[EncodedRect]) -> anyhow::Result<bool> {
        self.restore_cursor_backing();

        let fb_w = self.width as usize;
        let fb_h = self.height as usize;
        let mut need_pixel_fallback = false;

        for r in rects {
            let x = r.x as usize;
            let y = r.y as usize;
            let w = r.w as usize;
            let h = r.h as usize;
            if x >= fb_w || y >= fb_h || x + w > fb_w || y + h > fb_h {
                continue;
            }
            match r.encoding {
                RectEncoding::ShmDirtyOnly => {
                    let Some(mmap_len) = self.shm_mmap.as_ref().map(|m| m.len()) else {
                        need_pixel_fallback = true;
                        continue;
                    };
                    let shm_stride = self.stride as usize;
                    let base_offset = self.offset as usize;
                    let row_bytes = w * 4;
                    let mut oob = false;
                    {
                        let mmap = self.shm_mmap.as_ref().unwrap();
                        for dy in 0..h {
                            let Some(src_off) = (y + dy)
                                .checked_mul(shm_stride)
                                .and_then(|o| o.checked_add(base_offset))
                                .and_then(|o| o.checked_add(x * 4))
                            else {
                                oob = true;
                                break;
                            };
                            let Some(src_end) = src_off.checked_add(row_bytes) else {
                                oob = true;
                                break;
                            };
                            if src_end > mmap_len {
                                oob = true;
                                break;
                            }
                            let src_row = &mmap[src_off..src_end];
                            let dst_off = (y + dy) * fb_w + x;
                            for dx in 0..w {
                                let s = dx * 4;
                                let px = u32::from_le_bytes([
                                    src_row[s],
                                    src_row[s + 1],
                                    src_row[s + 2],
                                    0,
                                ]);
                                self.backing_fb[dst_off + dx] = px;
                                self.present_fb[dst_off + dx] = px;
                            }
                        }
                    }
                    if oob {
                        self.shm_mmap = None;
                        need_pixel_fallback = true;
                    }
                }
                RectEncoding::RawBgra | RectEncoding::Lz4Bgra => {
                    let raw = decode_bgra_rect(r.encoding, &r.data, w * h * 4)?;
                    let row_bytes = w * 4;
                    for dy in 0..h {
                        let src_row = &raw[dy * row_bytes..(dy + 1) * row_bytes];
                        let dst_off = (y + dy) * fb_w + x;
                        for dx in 0..w {
                            let s = dx * 4;
                            let px = u32::from_le_bytes([
                                src_row[s],
                                src_row[s + 1],
                                src_row[s + 2],
                                0,
                            ]);
                            self.backing_fb[dst_off + dx] = px;
                            self.present_fb[dst_off + dx] = px;
                        }
                    }
                }
            }
        }

        self.composite_cursor();
        Ok(need_pixel_fallback)
    }

    pub fn composite_cursor(&mut self) {
        self.restore_cursor_backing();
        if !self.cursor.visible
            || self.cursor.width == 0
            || self.cursor.height == 0
            || self.cursor.bgra.is_empty()
        {
            return;
        }
        // SynthVid PointerPositionMessage (and ServerMsg::CursorPosition) already
        // supplies the upper-left coordinates of the pointer image.
        let origin_x = self.cursor.x;
        let origin_y = self.cursor.y;
        let cw = self.cursor.width;
        let ch = self.cursor.height;
        let fb_w = self.width as i32;
        let fb_h = self.height as i32;

        for cy in 0..ch as i32 {
            let dst_y = origin_y + cy;
            if dst_y < 0 || dst_y >= fb_h {
                continue;
            }
            for cx in 0..cw as i32 {
                let dst_x = origin_x + cx;
                if dst_x < 0 || dst_x >= fb_w {
                    continue;
                }
                let src_idx = (cy as usize * cw as usize + cx as usize) * 4;
                let sb = self.cursor.bgra[src_idx] as u32;
                let sg = self.cursor.bgra[src_idx + 1] as u32;
                let sr = self.cursor.bgra[src_idx + 2] as u32;
                let sa = self.cursor.bgra[src_idx + 3] as u32;
                if sa == 0 {
                    continue;
                }
                let dst_idx = dst_y as usize * self.width as usize + dst_x as usize;
                if sa == 255 {
                    self.present_fb[dst_idx] = (sr << 16) | (sg << 8) | sb;
                } else {
                    let bg = self.backing_fb[dst_idx];
                    let inv = 255 - sa;
                    let br = (bg >> 16) & 0xff;
                    let bg_g = (bg >> 8) & 0xff;
                    let bb = bg & 0xff;
                    let r = (sr * sa + br * inv) / 255;
                    let g = (sg * sa + bg_g * inv) / 255;
                    let b = (sb * sa + bb * inv) / 255;
                    self.present_fb[dst_idx] = (r << 16) | (g << 8) | b;
                }
            }
        }
        self.cursor.prev_drawn = Some((origin_x, origin_y, cw, ch));
    }

    pub fn backing_checksum(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for &px in &self.backing_fb {
            h ^= px as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastdesk_proto::encode_bgra_rect;
    use std::io::Write;

    #[test]
    fn compositor_blits_rects_and_alpha_cursor_without_corrupting_backing() {
        let mut comp = DesktopCompositor::new(32, 32, 32 * 4, 0, None, false).unwrap();
        let mut raw = Vec::with_capacity(16 * 16 * 4);
        for _ in 0..16 * 16 {
            raw.extend_from_slice(&[0x10, 0x20, 0x30, 0xff]);
        }
        let (encoding, data) = encode_bgra_rect(&raw);
        comp.apply_rects(&[EncodedRect {
            x: 8,
            y: 8,
            w: 16,
            h: 16,
            encoding,
            data,
        }])
        .unwrap();
        assert_eq!(comp.backing_fb[8 * 32 + 8], 0x00302010);

        let cursor_raw = [0xffu8, 0x00, 0x00, 0xff].repeat(4); // opaque blue 2x2
        let cursor_lz4 = lz4_flex::compress_prepend_size(&cursor_raw);
        comp.set_cursor_shape(2, 2, 0, 0, &cursor_lz4).unwrap();
        comp.set_cursor_position(true, 10, 10);
        comp.composite_cursor();
        assert_eq!(comp.present_fb[10 * 32 + 10], 0x000000ff);
        assert_eq!(comp.backing_fb[10 * 32 + 10], 0x00302010);

        // Moving cursor restores previous spot cleanly.
        comp.set_cursor_position(true, 0, 0);
        comp.composite_cursor();
        assert_eq!(comp.present_fb[10 * 32 + 10], 0x00302010);
    }

    #[test]
    fn shm_compositor_reads_from_nonzero_surface_offset() {
        let offset = 4096usize;
        let width = 32u16;
        let height = 32u16;
        let stride = width as u32 * 4;
        let surface_bytes = width as usize * height as usize * 4;

        let mut vram = vec![0u8; offset + surface_bytes];
        let mut surface = Vec::with_capacity(surface_bytes);
        for _ in 0..(width as usize * height as usize) {
            surface.extend_from_slice(&[0x12, 0x34, 0x56, 0xff]);
        }
        vram[offset..].copy_from_slice(&surface);

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&vram).unwrap();
        tmp.flush().unwrap();
        let shm_path = tmp.path().to_string_lossy().into_owned();

        let mut shm_comp =
            DesktopCompositor::new(width, height, stride, offset as u32, Some(&shm_path), true)
                .unwrap();
        assert!(shm_comp.is_shm_active());
        let need_fallback = shm_comp
            .apply_rects(&[EncodedRect {
                x: 0,
                y: 0,
                w: width,
                h: height,
                encoding: RectEncoding::ShmDirtyOnly,
                data: Vec::new(),
            }])
            .unwrap();
        assert!(!need_fallback);
        assert_eq!(shm_comp.backing_fb[0], 0x00563412);
        assert_eq!(shm_comp.backing_fb[17 * 32 + 19], 0x00563412);

        let mut lz4_comp =
            DesktopCompositor::new(width, height, stride, offset as u32, None, false).unwrap();
        let (encoding, data) = encode_bgra_rect(&surface);
        lz4_comp
            .apply_rects(&[EncodedRect {
                x: 0,
                y: 0,
                w: width,
                h: height,
                encoding,
                data,
            }])
            .unwrap();
        assert_eq!(shm_comp.backing_checksum(), lz4_comp.backing_checksum());
    }

    #[test]
    fn shm_fallback_triggered_when_shm_missing_or_too_small_for_offset() {
        let missing = DesktopCompositor::new(
            32,
            32,
            32 * 4,
            0,
            Some("/nonexistent/fastdesk-missing.shm"),
            true,
        )
        .unwrap();
        assert!(!missing.is_shm_active());
        let mut missing = missing;
        let need_fallback = missing
            .apply_rects(&[EncodedRect {
                x: 0,
                y: 0,
                w: 32,
                h: 32,
                encoding: RectEncoding::ShmDirtyOnly,
                data: Vec::new(),
            }])
            .unwrap();
        assert!(need_fallback);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file().set_len(4096 + 64).unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let mut too_small =
            DesktopCompositor::new(32, 32, 32 * 4, 4096, Some(&path), true).unwrap();
        assert!(!too_small.is_shm_active());
        let need_fallback = too_small
            .apply_rects(&[EncodedRect {
                x: 0,
                y: 0,
                w: 32,
                h: 32,
                encoding: RectEncoding::ShmDirtyOnly,
                data: Vec::new(),
            }])
            .unwrap();
        assert!(need_fallback);
    }

    #[test]
    fn cursor_uses_supplied_upper_left_image_origin_without_reapplying_hotspot() {
        let mut comp = DesktopCompositor::new(16, 16, 16 * 4, 0, None, false).unwrap();

        // 2x2 opaque cursor with 4 distinct pixels and nonzero hotspot (1, 1).
        let p00 = [0x10u8, 0x20, 0x30, 0xff];
        let p10 = [0x40u8, 0x50, 0x60, 0xff];
        let p01 = [0x70u8, 0x80, 0x90, 0xff];
        let p11 = [0xa0u8, 0xb0, 0xc0, 0xff];
        let mut raw = Vec::with_capacity(16);
        raw.extend_from_slice(&p00);
        raw.extend_from_slice(&p10);
        raw.extend_from_slice(&p01);
        raw.extend_from_slice(&p11);
        let cursor_lz4 = lz4_flex::compress_prepend_size(&raw);

        comp.set_cursor_shape(2, 2, 1, 1, &cursor_lz4).unwrap();
        comp.set_cursor_position(true, 5, 6);
        comp.composite_cursor();

        // SynthVid supplies the upper-left image coordinate directly (5, 6),
        // so the cursor must NOT be shifted to (4, 5).
        assert_eq!(comp.present_fb[4 * 16 + 4], 0);
        assert_eq!(comp.present_fb[5 * 16 + 4], 0);
        assert_eq!(comp.present_fb[6 * 16 + 5], 0x00302010);
        assert_eq!(comp.present_fb[6 * 16 + 6], 0x00605040);
        assert_eq!(comp.present_fb[7 * 16 + 5], 0x00908070);
        assert_eq!(comp.present_fb[7 * 16 + 6], 0x00c0b0a0);

        // When the pointer tip is at (0, 0) with hotspot (1, 1), SynthVid reports
        // upper-left origin (-1, -1), placing pixel (1, 1) at (0, 0).
        comp.set_cursor_position(true, -1, -1);
        comp.composite_cursor();
        assert_eq!(comp.present_fb[6 * 16 + 5], 0);
        assert_eq!(comp.present_fb[6 * 16 + 6], 0);
        assert_eq!(comp.present_fb[7 * 16 + 5], 0);
        assert_eq!(comp.present_fb[7 * 16 + 6], 0);
        assert_eq!(comp.present_fb[0], 0x00c0b0a0);
    }

    #[test]
    fn cursor_shape_rejects_untrusted_size_prefix_before_allocating() {
        let mut comp = DesktopCompositor::new(16, 16, 16 * 4, 0, None, false).unwrap();

        // Set a valid cursor first to verify error paths leave existing cursor state intact.
        let valid_raw = [0x11u8, 0x22, 0x33, 0xff].repeat(4);
        let valid_lz4 = lz4_flex::compress_prepend_size(&valid_raw);
        comp.set_cursor_shape(2, 2, 0, 0, &valid_lz4).unwrap();

        // Five-byte payload claiming a 1 GiB uncompressed cursor.
        let mut one_gib_bomb = (1024u32 * 1024 * 1024).to_le_bytes().to_vec();
        one_gib_bomb.push(0);
        assert!(comp.set_cursor_shape(2, 2, 0, 0, &one_gib_bomb).is_err());

        // u32::MAX size prefix.
        let mut max_bomb = u32::MAX.to_le_bytes().to_vec();
        max_bomb.push(0);
        assert!(comp.set_cursor_shape(2, 2, 0, 0, &max_bomb).is_err());

        // Truncated (< 4 bytes) payload and invalid cursor dimensions.
        assert!(comp.set_cursor_shape(2, 2, 0, 0, &[1, 2, 3]).is_err());
        assert!(comp.set_cursor_shape(0, 2, 0, 0, &valid_lz4).is_err());
        assert!(comp.set_cursor_shape(97, 2, 0, 0, &valid_lz4).is_err());

        assert_eq!(comp.cursor.width, 2);
        assert_eq!(comp.cursor.height, 2);
        assert_eq!(comp.cursor.bgra, valid_raw);
    }
}
