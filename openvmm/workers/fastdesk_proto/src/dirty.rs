// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 16x16 dirty-tile bitmap, frame differencer, and rectangle coalescer.

/// Tile dimension in pixels (16x16).
pub const TILE_SIZE: u16 = 16;

/// An axis-aligned pixel rectangle in desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    /// Left X coordinate in pixels.
    pub x: u16,
    /// Top Y coordinate in pixels.
    pub y: u16,
    /// Width in pixels.
    pub w: u16,
    /// Height in pixels.
    pub h: u16,
}

/// Packed `Vec<u64>` bitmap tracking dirty 16x16 tiles across a framebuffer.
#[derive(Debug, Clone)]
pub struct DirtyTileTracker {
    words: Vec<u64>,
    num_tiles: usize,
    tiles_per_row: u16,
    tiles_per_col: u16,
    width: u16,
    height: u16,
}

impl DirtyTileTracker {
    /// Creates a new tracker for `width x height` pixels with all tiles marked dirty.
    pub fn new(width: u16, height: u16) -> Self {
        let tiles_per_row = width.div_ceil(TILE_SIZE);
        let tiles_per_col = height.div_ceil(TILE_SIZE);
        let num_tiles = tiles_per_row as usize * tiles_per_col as usize;
        let word_count = num_tiles.div_ceil(64);
        let mut tracker = Self {
            words: vec![0; word_count],
            num_tiles,
            tiles_per_row,
            tiles_per_col,
            width,
            height,
        };
        tracker.mark_all();
        tracker
    }

    /// Resizes the tracker for new dimensions and marks all tiles dirty.
    pub fn resize(&mut self, width: u16, height: u16) {
        *self = Self::new(width, height);
    }

    /// Current tracked width in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Current tracked height in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }

    #[inline]
    fn get_bit(words: &[u64], idx: usize) -> bool {
        (words[idx >> 6] >> (idx & 63)) & 1 != 0
    }

    #[inline]
    fn set_bit(words: &mut [u64], idx: usize) {
        words[idx >> 6] |= 1u64 << (idx & 63);
    }

    #[inline]
    fn clear_bit(words: &mut [u64], idx: usize) {
        words[idx >> 6] &= !(1u64 << (idx & 63));
    }

    fn set_range(&mut self, start: usize, end_inclusive: usize) {
        if start > end_inclusive || end_inclusive >= self.num_tiles {
            return;
        }
        let first_word = start >> 6;
        let last_word = end_inclusive >> 6;
        let start_bit = start & 63;
        let end_bit = end_inclusive & 63;

        if first_word == last_word {
            let mask = (!0u64 << start_bit) & (!0u64 >> (63 - end_bit));
            self.words[first_word] |= mask;
        } else {
            self.words[first_word] |= !0u64 << start_bit;
            for word in &mut self.words[first_word + 1..last_word] {
                *word = !0u64;
            }
            self.words[last_word] |= !0u64 >> (63 - end_bit);
        }
    }

    /// Marks all tiles overlapping `[left..right, top..bottom)` as dirty.
    pub fn mark_rect(&mut self, left: i32, top: i32, right: i32, bottom: i32) {
        let left = left.clamp(0, self.width as i32) as u16;
        let top = top.clamp(0, self.height as i32) as u16;
        let right = right.clamp(0, self.width as i32) as u16;
        let bottom = bottom.clamp(0, self.height as i32) as u16;

        if left >= right || top >= bottom {
            return;
        }

        let tile_left = left / TILE_SIZE;
        let tile_right = (right - 1) / TILE_SIZE;
        let tile_top = top / TILE_SIZE;
        let tile_bottom = (bottom - 1) / TILE_SIZE;

        for ty in tile_top..=tile_bottom {
            let row_start = ty as usize * self.tiles_per_row as usize;
            self.set_range(
                row_start + tile_left as usize,
                row_start + tile_right as usize,
            );
        }
    }

    /// Sets a single tile dirty by tile coordinates `(tile_x, tile_y)`.
    pub fn set_tile(&mut self, tile_x: u16, tile_y: u16) {
        if tile_x < self.tiles_per_row && tile_y < self.tiles_per_col {
            let idx = tile_y as usize * self.tiles_per_row as usize + tile_x as usize;
            Self::set_bit(&mut self.words, idx);
        }
    }

    /// Marks all tiles dirty.
    pub fn mark_all(&mut self) {
        if self.num_tiles == 0 {
            return;
        }
        self.words.fill(!0u64);
        let rem = self.num_tiles & 63;
        if rem != 0 {
            if let Some(last) = self.words.last_mut() {
                *last = (1u64 << rem) - 1;
            }
        }
    }

    /// Clears all dirty bits.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// Returns `true` if no tiles are dirty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Compares `cur_bgra` against `prev_bgra` in 16x16 tiles using slice equality
    /// and marks any differing tiles dirty.
    pub fn diff_frames(&mut self, prev_bgra: &[u8], cur_bgra: &[u8], stride_bytes: usize) {
        let w = self.width as usize;
        let h = self.height as usize;
        if w == 0 || h == 0 || stride_bytes < w * 4 {
            return;
        }
        let needed = (h - 1) * stride_bytes + w * 4;
        if prev_bgra.len() < needed || cur_bgra.len() < needed {
            self.mark_all();
            return;
        }

        let tile_px = TILE_SIZE as usize;
        for ty in 0..self.tiles_per_col {
            let y0 = ty as usize * tile_px;
            let th = tile_px.min(h - y0);
            for tx in 0..self.tiles_per_row {
                let x0 = tx as usize * tile_px;
                let tw_bytes = tile_px.min(w - x0) * 4;
                let mut changed = false;
                for dy in 0..th {
                    let off = (y0 + dy) * stride_bytes + x0 * 4;
                    if prev_bgra[off..off + tw_bytes] != cur_bgra[off..off + tw_bytes] {
                        changed = true;
                        break;
                    }
                }
                if changed {
                    self.set_tile(tx, ty);
                }
            }
        }
    }

    /// Merges dirty tiles into coalesced rectangles (horizontal span + vertical
    /// expansion) and writes them into `out`.
    pub fn merge_into(&self, out: &mut Vec<TileRect>) {
        out.clear();
        if self.is_empty() {
            return;
        }
        let mut scratch = self.words.clone();
        let tiles_per_row = self.tiles_per_row as usize;
        let tiles_per_col = self.tiles_per_col as usize;

        for ty in 0..tiles_per_col {
            let row_base = ty * tiles_per_row;
            let mut tx = 0;
            while tx < tiles_per_row {
                if !Self::get_bit(&scratch, row_base + tx) {
                    tx += 1;
                    continue;
                }
                let run_start = tx;
                let mut run_end = tx + 1;
                while run_end < tiles_per_row && Self::get_bit(&scratch, row_base + run_end) {
                    run_end += 1;
                }

                let mut h_tiles = 1usize;
                'extend: while ty + h_tiles < tiles_per_col {
                    let below = (ty + h_tiles) * tiles_per_row;
                    for c in run_start..run_end {
                        if !Self::get_bit(&scratch, below + c) {
                            break 'extend;
                        }
                    }
                    h_tiles += 1;
                }

                let x_u32 = run_start as u32 * TILE_SIZE as u32;
                let y_u32 = ty as u32 * TILE_SIZE as u32;
                let right_u32 = (run_end as u32 * TILE_SIZE as u32).min(self.width as u32);
                let bottom_u32 = ((ty + h_tiles) as u32 * TILE_SIZE as u32).min(self.height as u32);

                out.push(TileRect {
                    x: x_u32 as u16,
                    y: y_u32 as u16,
                    w: (right_u32 - x_u32) as u16,
                    h: (bottom_u32 - y_u32) as u16,
                });

                for dy in 0..h_tiles {
                    let row = (ty + dy) * tiles_per_row;
                    for c in run_start..run_end {
                        Self::clear_bit(&mut scratch, row + c);
                    }
                }
                tx = run_end;
            }
        }
    }
}
