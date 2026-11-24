use crate::compositor::DesktopCompositor;
use crate::transport::TransportStream;
use fastdesk_proto::CAP_ALL;
use fastdesk_proto::ClientMsg;
use fastdesk_proto::DirtyTileTracker;
use fastdesk_proto::EncodedRect;
use fastdesk_proto::RectEncoding;
use fastdesk_proto::ServerMsg;
use fastdesk_proto::TileRect;
use fastdesk_proto::encode_bgra_rect;
use fastdesk_proto::encode_server_msg;
use fastdesk_proto::read_client_msg;
use fastdesk_proto::read_server_msg;
use fastdesk_proto::write_client_msg;
use fastdesk_proto::write_server_msg;
use memmap2::MmapMut;
use std::io::Write;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;
use std::time::Instant;

struct BenchResult {
    label: &'static str,
    frames: usize,
    total_time: Duration,
    wire_bytes: u64,
    raw_pixel_bytes: u64,
    p50_us: u64,
    p99_us: u64,
    checksum: u64,
    expected_checksum: u64,
}

fn fill_rect(buf: &mut [u8], width: u16, x: u16, y: u16, w: u16, h: u16, b: u8, g: u8, r: u8) {
    let stride = width as usize * 4;
    for dy in 0..h as usize {
        let row = (y as usize + dy) * stride + x as usize * 4;
        for dx in 0..w as usize {
            let off = row + dx * 4;
            buf[off] = b.wrapping_add((dx & 0x0f) as u8);
            buf[off + 1] = g.wrapping_add((dy & 0x0f) as u8);
            buf[off + 2] = r;
            buf[off + 3] = 0xff;
        }
    }
}

fn bgra_checksum(buf: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for px in buf.chunks_exact(4) {
        let v = u32::from_le_bytes([px[0], px[1], px[2], 0]) as u64;
        h ^= v;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn run_single_self_test(
    label: &'static str,
    prefer_shm: bool,
    frames: usize,
    width: u16,
    height: u16,
) -> anyhow::Result<BenchResult> {
    let stride = width as u32 * 4;
    let vram_size = width as usize * height as usize * 4;
    let tmp = tempfile::NamedTempFile::new()?;
    tmp.as_file().set_len(vram_size as u64)?;
    let shm_path_str = tmp.path().to_string_lossy().into_owned();

    // SAFETY: exclusive test temporary file mapped for shared VRAM benchmark.
    let mut server_vram = unsafe { MmapMut::map_mut(tmp.as_file())? };
    fill_rect(&mut server_vram, width, 0, 0, width, height, 0x20, 0x24, 0x2b);
    server_vram.flush()?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    let server_thread = thread::spawn(move || -> anyhow::Result<u64> {
        let (mut sock, _) = listener.accept()?;
        sock.set_nodelay(true)?;
        let start = Instant::now();

        write_server_msg(
            &mut sock,
            &ServerMsg::ServerHello {
                capabilities: CAP_ALL,
                width,
                height,
                stride,
                offset: 0,
                vram_size: vram_size as u32,
                shm_path: Some(shm_path_str),
            },
        )?;

        let use_shm = match read_client_msg(&mut sock)? {
            ClientMsg::ClientHello { prefer_shm, .. } => prefer_shm,
            other => anyhow::bail!("expected ClientHello, got {other:?}"),
        };

        // Send initial hardware cursor shape.
        let cursor_px = vec![0xffu8; 24 * 24 * 4];
        write_server_msg(
            &mut sock,
            &ServerMsg::CursorShape {
                width: 24,
                height: 24,
                hotspot_x: 1,
                hotspot_y: 1,
                lz4_bgra: lz4_flex::compress_prepend_size(&cursor_px),
            },
        )?;

        let mut tracker = DirtyTileTracker::new(width, height);
        let mut merged: Vec<TileRect> = Vec::new();
        let mut out = Vec::with_capacity(256 * 1024);
        let mut raw_scratch = Vec::new();

        for seq in 0..frames {
            out.clear();
            tracker.clear();

            if seq == 0 {
                tracker.mark_all();
            } else {
                // Realistic 2D desktop mix: typing caret, window drag panel, smooth scroll band.
                match seq % 3 {
                    0 => {
                        let x = ((seq * 16) as u16) % (width - 64);
                        let y = 120;
                        fill_rect(&mut server_vram, width, x, y, 48, 32, 0xe0, 0xe0, 0xe0);
                        tracker.mark_rect(x as i32, y as i32, (x + 48) as i32, (y + 32) as i32);
                    }
                    1 => {
                        let x = ((seq * 23) as u16) % (width - 360);
                        let y = ((seq * 11) as u16) % (height - 260);
                        fill_rect(&mut server_vram, width, x, y, 360, 240, 0x45, 0x80, 0xc0);
                        tracker.mark_rect(x as i32, y as i32, (x + 360) as i32, (y + 240) as i32);
                    }
                    _ => {
                        let y = ((seq * 32) as u16) % (height - 80);
                        fill_rect(&mut server_vram, width, 40, y, width - 80, 64, 0xf2, 0xf2, 0xf5);
                        tracker.mark_rect(40, y as i32, (width - 40) as i32, (y + 64) as i32);
                    }
                }
            }

            tracker.merge_into(&mut merged);
            let mut rects = Vec::with_capacity(merged.len());
            for r in &merged {
                if use_shm {
                    rects.push(EncodedRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        encoding: RectEncoding::ShmDirtyOnly,
                        data: Vec::new(),
                    });
                } else {
                    let row_bytes = r.w as usize * 4;
                    raw_scratch.resize(row_bytes * r.h as usize, 0);
                    for dy in 0..r.h as usize {
                        let src = (r.y as usize + dy) * stride as usize + r.x as usize * 4;
                        raw_scratch[dy * row_bytes..(dy + 1) * row_bytes]
                            .copy_from_slice(&server_vram[src..src + row_bytes]);
                    }
                    let (encoding, data) = encode_bgra_rect(&raw_scratch);
                    rects.push(EncodedRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        encoding,
                        data,
                    });
                }
            }

            encode_server_msg(
                &ServerMsg::CursorPosition {
                    visible: true,
                    x: ((seq * 7) as i32) % width as i32,
                    y: ((seq * 5) as i32) % height as i32,
                },
                &mut out,
            )?;
            encode_server_msg(
                &ServerMsg::FrameBatch {
                    frame_seq: seq as u64 + 1,
                    host_time_us: start.elapsed().as_micros() as u64,
                    rects,
                },
                &mut out,
            )?;
            sock.write_all(&out)?;
        }

        // Wait for final Ping from client so we verify bidirectional input/control too.
        match read_client_msg(&mut sock)? {
            ClientMsg::Ping { client_time_us } => {
                write_server_msg(
                    &mut sock,
                    &ServerMsg::Pong {
                        client_time_us,
                        host_time_us: start.elapsed().as_micros() as u64,
                    },
                )?;
            }
            other => anyhow::bail!("expected Ping, got {other:?}"),
        }

        Ok(bgra_checksum(&server_vram))
    });

    let mut stream = TransportStream::connect(&format!("tcp://{addr}"))?;
    let (w, h, s, offset, shm_path) = match read_server_msg(&mut stream)? {
        ServerMsg::ServerHello {
            width,
            height,
            stride,
            offset,
            shm_path,
            ..
        } => (width, height, stride, offset, shm_path),
        other => anyhow::bail!("expected ServerHello, got {other:?}"),
    };

    write_client_msg(
        &mut stream,
        &ClientMsg::ClientHello {
            capabilities: CAP_ALL,
            prefer_shm,
        },
    )?;

    let mut comp = DesktopCompositor::new(w, h, s, offset, shm_path.as_deref(), prefer_shm)?;
    let mut latencies_us = Vec::with_capacity(frames);
    let mut wire_bytes = 0u64;
    let mut raw_pixel_bytes = 0u64;
    let mut got_frames = 0usize;

    let bench_start = Instant::now();
    while got_frames < frames {
        match read_server_msg(&mut stream)? {
            ServerMsg::CursorShape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                lz4_bgra,
            } => {
                comp.set_cursor_shape(width, height, hotspot_x, hotspot_y, &lz4_bgra)?;
            }
            ServerMsg::CursorPosition { visible, x, y } => {
                comp.set_cursor_position(visible, x, y);
            }
            ServerMsg::FrameBatch { rects, .. } => {
                let t0 = Instant::now();
                for r in &rects {
                    wire_bytes += 13 + r.data.len() as u64;
                    raw_pixel_bytes += r.w as u64 * r.h as u64 * 4;
                }
                if comp.apply_rects(&rects)? {
                    anyhow::bail!("unexpected SHM fallback needed during SHM test");
                }
                latencies_us.push(t0.elapsed().as_micros() as u64);
                got_frames += 1;
            }
            other => anyhow::bail!("unexpected msg during bench: {other:?}"),
        }
    }
    let total_time = bench_start.elapsed();

    write_client_msg(
        &mut stream,
        &ClientMsg::Ping {
            client_time_us: 12345,
        },
    )?;
    match read_server_msg(&mut stream)? {
        ServerMsg::Pong {
            client_time_us: 12345,
            ..
        } => {}
        other => anyhow::bail!("expected Pong, got {other:?}"),
    }

    let expected_checksum = server_thread.join().unwrap()?;
    let checksum = comp.backing_checksum();
    anyhow::ensure!(
        checksum == expected_checksum,
        "{label} checksum mismatch: viewer={checksum:#x}, server={expected_checksum:#x}"
    );

    latencies_us.sort_unstable();
    let p50_us = latencies_us[latencies_us.len() / 2];
    let p99_us = latencies_us[((latencies_us.len() * 99) / 100).min(latencies_us.len() - 1)];

    Ok(BenchResult {
        label,
        frames,
        total_time,
        wire_bytes,
        raw_pixel_bytes,
        p50_us,
        p99_us,
        checksum,
        expected_checksum,
    })
}

pub fn run_self_test(frames: usize, width: u16, height: u16) -> anyhow::Result<()> {
    println!("FastDesk Self-Test Benchmark ({width}x{height}, {frames} frames per mode)");
    let lz4 = run_single_self_test("LZ4 Tile (TCP)", false, frames, width, height)?;
    let shm = run_single_self_test("SHM Zero-Copy", true, frames, width, height)?;

    for res in [&lz4, &shm] {
        let secs = res.total_time.as_secs_f64().max(1e-6);
        let fps = res.frames as f64 / secs;
        let wire_mib = res.wire_bytes as f64 / (1024.0 * 1024.0);
        let raw_mib = res.raw_pixel_bytes as f64 / (1024.0 * 1024.0);
        let ratio = if res.wire_bytes > 0 {
            res.raw_pixel_bytes as f64 / res.wire_bytes as f64
        } else {
            f64::INFINITY
        };
        assert_eq!(res.checksum, res.expected_checksum);
        println!(
            "  {:<16} | {:>7.1} FPS | total {:>6.2} ms | apply P50 {:>4} us P99 {:>4} us | wire {:>6.2} MiB / raw {:>6.2} MiB ({:.1}x) | CHECKSUM OK ({:#018x})",
            res.label,
            fps,
            res.total_time.as_secs_f64() * 1000.0,
            res.p50_us,
            res.p99_us,
            wire_mib,
            raw_mib,
            ratio,
            res.checksum
        );
    }
    Ok(())
}

pub fn run_remote_bench(endpoint: &str, frames: usize, allow_shm: bool) -> anyhow::Result<()> {
    let mut stream = TransportStream::connect(endpoint)?;
    let (w, h, s, offset, shm_path) = match read_server_msg(&mut stream)? {
        ServerMsg::ServerHello {
            width,
            height,
            stride,
            offset,
            shm_path,
            ..
        } => (width, height, stride, offset, shm_path),
        other => anyhow::bail!("expected ServerHello, got {other:?}"),
    };
    let mut comp = DesktopCompositor::new(w, h, s, offset, shm_path.as_deref(), allow_shm)?;
    write_client_msg(
        &mut stream,
        &ClientMsg::ClientHello {
            capabilities: CAP_ALL,
            prefer_shm: comp.is_shm_active(),
        },
    )?;

    let mut rtts_us = Vec::new();
    let start = Instant::now();
    let mut got_frames = 0usize;
    let mut wire_bytes = 0u64;

    while got_frames < frames {
        let ping_sent = Instant::now();
        write_client_msg(
            &mut stream,
            &ClientMsg::Ping {
                client_time_us: ping_sent.elapsed().as_micros() as u64,
            },
        )?;
        write_client_msg(
            &mut stream,
            &ClientMsg::RequestFullFrame {
                allow_shm: comp.is_shm_active(),
            },
        )?;
        loop {
            match read_server_msg(&mut stream)? {
                ServerMsg::Pong { .. } => {
                    rtts_us.push(ping_sent.elapsed().as_micros() as u64);
                }
                ServerMsg::FrameBatch { rects, .. } => {
                    for r in &rects {
                        wire_bytes += 13 + r.data.len() as u64;
                    }
                    let _ = comp.apply_rects(&rects)?;
                    got_frames += 1;
                    break;
                }
                ServerMsg::Resolution {
                    width,
                    height,
                    stride,
                    offset,
                    shm_path,
                } => comp.resize(width, height, stride, offset, shm_path.as_deref(), allow_shm)?,
                ServerMsg::CursorShape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    lz4_bgra,
                } => comp.set_cursor_shape(width, height, hotspot_x, hotspot_y, &lz4_bgra)?,
                ServerMsg::CursorPosition { visible, x, y } => {
                    comp.set_cursor_position(visible, x, y)
                }
                ServerMsg::ServerHello { .. } => {}
            }
        }
    }

    rtts_us.sort_unstable();
    let p50 = rtts_us.get(rtts_us.len() / 2).copied().unwrap_or(0);
    let p99 = rtts_us
        .get(((rtts_us.len() * 99) / 100).min(rtts_us.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0);
    println!(
        "Remote FastDesk Bench ({endpoint}, {}x{}, shm={}): {frames} frames in {:.2}s ({:.1} FPS), ping P50={p50} us P99={p99} us, {:.2} MiB received",
        comp.width,
        comp.height,
        comp.is_shm_active(),
        start.elapsed().as_secs_f64(),
        frames as f64 / start.elapsed().as_secs_f64().max(1e-6),
        wire_bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}
