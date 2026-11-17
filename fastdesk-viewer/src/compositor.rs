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
    hotspot_x: u16,
    hotspot_y: u16,
    bgra: Vec<u8>,
    prev_drawn: Option<(i32, i32, u16, u16)>,
}

pub struct DesktopCompositor {
    pub width: u16,
    pub height: u16,
    pub stride: u32,
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
        shm_path: Option<&str>,
        allow_shm: bool,
    ) -> anyhow::Result<Self> {
        let len = width as usize * height as usize;
        let mut comp = Self {
            width,
            height,
            stride,
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
        if let Ok(file) = File::open(path) {
            // SAFETY: file is opened read-only and maps shared guest VRAM maintained by OpenVMM.
            if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                self.shm_mmap = Some(mmap);
            }
        }
    }

    pub fn resize(&mut self, width: u16, height: u16, stride: u32, shm_path: Option<&str>, allow_shm: bool) {
        self.width = width;
        self.height = height;
        self.stride = stride;
        let len = width as usize * height as usize;
        self.backing_fb.resize(len, 0);
        self.backing_fb.fill(0);
        self.present_fb.resize(len, 0);
        self.present_fb.fill(0);
        self.cursor.prev_drawn = None;
        if allow_shm {
            if let Some(path) = shm_path {
                self.try_open_shm(path);
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
        hotspot_x: u16,
        hotspot_y: u16,
        lz4_bgra: &[u8],
    ) -> anyhow::Result<()> {
        let expected = width as usize * height as usize * 4;
        let decoded = lz4_flex::decompress_size_prepended(lz4_bgra)
            .map_err(|e| anyhow::anyhow!("cursor decompress error: {e}"))?;
        if decoded.len() != expected {
            anyhow::bail!("cursor size mismatch: expected {expected}, got {}", decoded.len());
        }
        self.cursor.width = width;
        self.cursor.height = height;
        self.cursor.hotspot_x = hotspot_x;
        self.cursor.hotspot_y = hotspot_y;
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
                    let Some(mmap) = &self.shm_mmap else {
                        need_pixel_fallback = true;
                        continue;
                    };
                    let shm_stride = self.stride as usize;
                    for dy in 0..h {
                        let src_off = (y + dy) * shm_stride + x * 4;
                        let row_bytes = w * 4;
                        if src_off + row_bytes > mmap.len() {
                            break;
                        }
                        let src_row = &mmap[src_off..src_off + row_bytes];
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
        let origin_x = self.cursor.x - self.cursor.hotspot_x as i32;
        let origin_y = self.cursor.y - self.cursor.hotspot_y as i32;
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

    #[test]
    fn compositor_blits_rects_and_alpha_cursor_without_corrupting_backing() {
        let mut comp = DesktopCompositor::new(32, 32, 32 * 4, None, false).unwrap();
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
}
