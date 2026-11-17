mod bench;
mod compositor;
mod input;
mod transport;

use anyhow::Context;
use clap::Parser;
use compositor::DesktopCompositor;
use fastdesk_proto::CAP_ALL;
use fastdesk_proto::ClientMsg;
use fastdesk_proto::ServerMsg;
use fastdesk_proto::read_server_msg;
use fastdesk_proto::write_client_msg;
use input::InputTracker;
use minifb::ScaleMode;
use minifb::Window;
use minifb::WindowOptions;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use transport::TransportStream;

#[derive(Parser)]
#[command(name = "fastdesk-viewer", about = "Low-latency FastDesk viewer for OpenVMM and Hyper-V")]
struct Args {
    /// Target endpoint: tcp://host:port, hvsock://<vm_guid>/<port_or_guid>, or vsock://<cid>:<port>
    #[arg(long, default_value = "tcp://127.0.0.1:5901")]
    connect: String,

    /// Disable zero-copy shared memory even if the server exposes a local SHM file
    #[arg(long)]
    no_shm: bool,

    /// Target maximum UI present FPS (0 = uncapped)
    #[arg(long, default_value_t = 144)]
    max_fps: u64,

    /// Window title prefix
    #[arg(long, default_value = "FastDesk Viewer")]
    title: String,

    /// Run headless benchmark mode instead of opening a desktop window
    #[arg(long)]
    bench: bool,

    /// With --bench, run an in-process loopback self-test covering LZ4 and SHM Zero-Copy
    #[arg(long)]
    self_test: bool,

    /// Number of frames for --bench
    #[arg(long, default_value_t = 300)]
    frames: usize,

    /// Resolution WxH for --bench --self-test
    #[arg(long, default_value = "1920x1080")]
    resolution: String,
}

fn parse_resolution(s: &str) -> anyhow::Result<(u16, u16)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("resolution must be WxH, got '{s}'"))?;
    Ok((w.parse()?, h.parse()?))
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    if args.bench {
        if args.self_test {
            let (w, h) = parse_resolution(&args.resolution)?;
            return bench::run_self_test(args.frames, w, h);
        }
        return bench::run_remote_bench(&args.connect, args.frames, !args.no_shm);
    }

    run_gui(args)
}

fn run_gui(args: Args) -> anyhow::Result<()> {
    let allow_shm = !args.no_shm;
    let mut reader_stream = TransportStream::connect(&args.connect)
        .with_context(|| format!("connecting to {}", args.connect))?;
    let mut writer_stream = reader_stream.try_clone()?;

    let (init_w, init_h, init_stride, shm_path) = match read_server_msg(&mut reader_stream)? {
        ServerMsg::ServerHello {
            width,
            height,
            stride,
            shm_path,
            ..
        } => (width, height, stride, shm_path),
        other => anyhow::bail!("expected ServerHello, got {other:?}"),
    };

    write_client_msg(
        &mut writer_stream,
        &ClientMsg::ClientHello {
            capabilities: CAP_ALL,
            prefer_shm: allow_shm,
        },
    )?;

    let mut comp = DesktopCompositor::new(init_w, init_h, init_stride, shm_path.as_deref(), allow_shm)?;

    let (tx, rx) = mpsc::channel::<anyhow::Result<ServerMsg>>();
    thread::spawn(move || {
        loop {
            match read_server_msg(&mut reader_stream) {
                Ok(msg) => {
                    if tx.send(Ok(msg)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err.into()));
                    break;
                }
            }
        }
    });

    let mut window = Window::new(
        &args.title,
        comp.width as usize,
        comp.height as usize,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )?;
    if args.max_fps > 0 {
        window.set_target_fps(args.max_fps as usize);
    } else {
        window.set_target_fps(0);
    }

    let mut input = InputTracker::default();
    let mut frame_count = 0u64;
    let mut stat_window_start = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_rtt_us = 0u64;
    let mut dirty = true;

    while window.is_open() {
        while let Ok(item) = rx.try_recv() {
            match item? {
                ServerMsg::ServerHello { .. } => {}
                ServerMsg::Resolution {
                    width,
                    height,
                    stride,
                    shm_path,
                } => {
                    comp.resize(width, height, stride, shm_path.as_deref(), allow_shm);
                    window = Window::new(
                        &args.title,
                        comp.width as usize,
                        comp.height as usize,
                        WindowOptions {
                            resize: true,
                            scale_mode: ScaleMode::AspectRatioStretch,
                            ..WindowOptions::default()
                        },
                    )?;
                    if args.max_fps > 0 {
                        window.set_target_fps(args.max_fps as usize);
                    }
                    dirty = true;
                }
                ServerMsg::FrameBatch { rects, .. } => {
                    if comp.apply_rects(&rects)? {
                        write_client_msg(
                            &mut writer_stream,
                            &ClientMsg::RequestFullFrame { allow_shm: false },
                        )?;
                    }
                    frame_count += 1;
                    dirty = true;
                }
                ServerMsg::CursorPosition { visible, x, y } => {
                    comp.set_cursor_position(visible, x, y);
                    comp.composite_cursor();
                    dirty = true;
                }
                ServerMsg::CursorShape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    lz4_bgra,
                } => {
                    comp.set_cursor_shape(width, height, hotspot_x, hotspot_y, &lz4_bgra)?;
                    comp.composite_cursor();
                    dirty = true;
                }
                ServerMsg::Pong { client_time_us, .. } => {
                    let now_us = stat_window_start.elapsed().as_micros() as u64;
                    last_rtt_us = now_us.saturating_sub(client_time_us);
                }
            }
        }

        for msg in input.poll(&window, comp.width, comp.height) {
            write_client_msg(&mut writer_stream, &msg)?;
        }

        if last_ping.elapsed() >= Duration::from_secs(1) {
            last_ping = Instant::now();
            write_client_msg(
                &mut writer_stream,
                &ClientMsg::Ping {
                    client_time_us: stat_window_start.elapsed().as_micros() as u64,
                },
            )?;
        }

        if stat_window_start.elapsed() >= Duration::from_secs(1) {
            let secs = stat_window_start.elapsed().as_secs_f64().max(1e-6);
            let fps = frame_count as f64 / secs;
            let mode = if comp.is_shm_active() { "SHM Zero-Copy" } else { "LZ4 Tile" };
            window.set_title(&format!(
                "{} — {}x{} [{}] | {:.1} FPS | RTT {} us",
                args.title, comp.width, comp.height, mode, fps, last_rtt_us
            ));
            frame_count = 0;
            stat_window_start = Instant::now();
        }

        if dirty {
            window.update_with_buffer(&comp.present_fb, comp.width as usize, comp.height as usize)?;
            dirty = false;
        } else {
            window.update();
        }
    }

    Ok(())
}
