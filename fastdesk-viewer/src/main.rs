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

const SERVER_MSG_QUEUE_CAPACITY: usize = 16;
const MAX_SERVER_MSGS_PER_TICK: usize = 16;
const MAX_SERVER_DRAIN_TIME_PER_TICK: Duration = Duration::from_millis(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    Empty,
    BudgetExhausted,
    Disconnected,
}

fn spawn_server_reader<R: std::io::Read + Send + 'static>(
    mut reader: R,
    capacity: usize,
) -> mpsc::Receiver<anyhow::Result<ServerMsg>> {
    let (tx, rx) = mpsc::sync_channel::<anyhow::Result<ServerMsg>>(capacity.max(1));
    thread::spawn(move || {
        loop {
            match read_server_msg(&mut reader) {
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
    rx
}

fn drain_server_messages<F>(
    rx: &mpsc::Receiver<anyhow::Result<ServerMsg>>,
    max_msgs: usize,
    max_duration: Duration,
    mut handle: F,
) -> anyhow::Result<DrainOutcome>
where
    F: FnMut(ServerMsg) -> anyhow::Result<()>,
{
    let limit = max_msgs.max(1);
    let start = Instant::now();
    let mut processed = 0usize;
    loop {
        match rx.try_recv() {
            Ok(item) => {
                handle(item?)?;
                processed += 1;
                if processed >= limit || start.elapsed() >= max_duration {
                    return Ok(DrainOutcome::BudgetExhausted);
                }
            }
            Err(mpsc::TryRecvError::Empty) => return Ok(DrainOutcome::Empty),
            Err(mpsc::TryRecvError::Disconnected) => return Ok(DrainOutcome::Disconnected),
        }
    }
}

#[cfg(any(windows, test))]
const WIN32_HTCLIENT: u32 = 1;

#[cfg(any(windows, test))]
fn should_hide_win32_cursor_for_setcursor(
    hide_client_cursor: bool,
    target_hwnd: usize,
    wparam_hwnd: usize,
    lparam: isize,
) -> bool {
    let hit_test = (lparam as usize & 0xffff) as u32;
    hide_client_cursor
        && target_hwnd != 0
        && target_hwnd == wparam_hwnd
        && hit_test == WIN32_HTCLIENT
}

#[cfg(windows)]
mod win32_cursor {
    use super::should_hide_win32_cursor_for_setcursor;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::Foundation::LRESULT;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Foundation::WPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::CallWindowProcW;
    use windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    use windows_sys::Win32::UI::WindowsAndMessaging::GWLP_WNDPROC;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursor;
    use windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_NCDESTROY;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_NCHITTEST;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_SETCURSOR;
    use windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC;
    use windows_sys::Win32::UI::WindowsAndMessaging::WindowFromPoint;

    #[derive(Clone, Copy)]
    struct SubclassEntry {
        prev_proc: isize,
        hide_client_cursor: bool,
    }

    fn registry() -> &'static Mutex<HashMap<usize, SubclassEntry>> {
        static REGISTRY: OnceLock<Mutex<HashMap<usize, SubclassEntry>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn proc_addr() -> isize {
        client_cursor_wnd_proc as *const () as usize as isize
    }

    fn as_wndproc(addr: isize) -> WNDPROC {
        if addr == 0 {
            None
        } else {
            // SAFETY: `addr` was obtained from GetWindowLongPtrW/SetWindowLongPtrW(GWLP_WNDPROC).
            Some(unsafe {
                std::mem::transmute::<
                    isize,
                    unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                >(addr)
            })
        }
    }

    unsafe extern "system" fn client_cursor_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let key = hwnd as usize;
        let our_addr = proc_addr();
        let entry = {
            let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
            if msg == WM_NCDESTROY {
                map.remove(&key)
            } else {
                map.get(&key).copied()
            }
        };

        if msg == WM_SETCURSOR {
            if let Some(e) = entry {
                if should_hide_win32_cursor_for_setcursor(e.hide_client_cursor, key, wparam, lparam)
                {
                    unsafe {
                        SetCursor(std::ptr::null_mut());
                    }
                    return 1;
                }
            }
        }

        if let Some(e) = entry {
            if e.prev_proc != 0 && e.prev_proc != our_addr {
                return unsafe {
                    CallWindowProcW(as_wndproc(e.prev_proc), hwnd, msg, wparam, lparam)
                };
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn refresh_cursor_now(hwnd: HWND) {
        unsafe {
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt) != 0 && WindowFromPoint(pt) == hwnd {
                let packed_x = (pt.x as i16 as u16) as usize;
                let packed_y = (pt.y as i16 as u16) as usize;
                let hit_lparam = ((packed_y << 16) | packed_x) as LPARAM;
                let hit = SendMessageW(hwnd, WM_NCHITTEST, 0, hit_lparam);
                if hit >= 0 {
                    let setcursor_lparam = (hit as usize & 0xffff) as LPARAM;
                    let _ = SendMessageW(hwnd, WM_SETCURSOR, hwnd as usize, setcursor_lparam);
                }
            }
        }
    }

    pub fn set_client_cursor_visibility(hwnd: HWND, visible: bool) {
        if hwnd.is_null() {
            return;
        }
        let key = hwnd as usize;
        let our_addr = proc_addr();
        let hide = !visible;

        if hide {
            {
                let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = map.get_mut(&key) {
                    entry.hide_client_cursor = true;
                    drop(map);
                    refresh_cursor_now(hwnd);
                    return;
                }
            }

            let prev = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
            if prev == 0 || prev == our_addr {
                return;
            }
            {
                let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
                map.insert(
                    key,
                    SubclassEntry {
                        prev_proc: prev,
                        hide_client_cursor: true,
                    },
                );
            }
            let actual_prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, our_addr) };
            {
                let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
                if actual_prev == 0 {
                    map.remove(&key);
                    return;
                }
                if actual_prev != our_addr {
                    if let Some(entry) = map.get_mut(&key) {
                        entry.prev_proc = actual_prev;
                    }
                }
            }
            refresh_cursor_now(hwnd);
        } else {
            let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
            let prev_to_restore = {
                let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
                if current == our_addr {
                    map.remove(&key).map(|e| e.prev_proc)
                } else {
                    if let Some(entry) = map.get_mut(&key) {
                        entry.hide_client_cursor = false;
                    }
                    None
                }
            };
            if let Some(prev) = prev_to_restore {
                if prev != 0 && prev != our_addr {
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, prev);
                    }
                }
            }
            refresh_cursor_now(hwnd);
        }
    }
}

trait CursorVisibilityTarget {
    fn set_native_cursor_visibility(&mut self, visible: bool);
}

impl CursorVisibilityTarget for Window {
    fn set_native_cursor_visibility(&mut self, visible: bool) {
        #[cfg(windows)]
        {
            let hwnd = self.get_window_handle() as windows_sys::Win32::Foundation::HWND;
            win32_cursor::set_client_cursor_visibility(hwnd, visible);
        }
        #[cfg(not(windows))]
        {
            self.set_cursor_visibility(visible);
        }
    }
}

#[derive(Debug, Default)]
struct NativeCursorController {
    hidden: bool,
}

impl NativeCursorController {
    fn sync(&mut self, window: &mut impl CursorVisibilityTarget, hide: bool) {
        if hide == self.hidden {
            return;
        }
        window.set_native_cursor_visibility(!hide);
        self.hidden = hide;
    }

    fn on_window_created(&mut self, window: &mut impl CursorVisibilityTarget) {
        self.sync(window, true);
    }

    fn before_window_recreate(&mut self, old_window: &mut impl CursorVisibilityTarget) {
        self.sync(old_window, false);
    }

    fn restore(&mut self, window: &mut impl CursorVisibilityTarget) {
        self.sync(window, false);
    }
}

fn create_viewer_window(
    title: &str,
    width: usize,
    height: usize,
    max_fps: u64,
    input: &mut InputTracker,
    native_cursor: &mut NativeCursorController,
) -> anyhow::Result<Window> {
    let mut window = Window::new(
        title,
        width,
        height,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )?;
    if max_fps > 0 {
        window.set_target_fps(max_fps as usize);
    } else {
        window.set_target_fps(0);
    }
    native_cursor.on_window_created(&mut window);
    input.attach_window(&mut window);
    Ok(window)
}

fn detect_window_resize(last_size: &mut (usize, usize), current_size: (usize, usize)) -> bool {
    if current_size != *last_size {
        *last_size = current_size;
        true
    } else {
        false
    }
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

    let (init_w, init_h, init_stride, init_offset, shm_path) = match read_server_msg(&mut reader_stream)? {
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

    let mut comp = DesktopCompositor::new(
        init_w,
        init_h,
        init_stride,
        init_offset,
        shm_path.as_deref(),
        allow_shm,
    )?;
    let mut shm_enabled = comp.is_shm_active();
    let mut fallback_pending = false;

    write_client_msg(
        &mut writer_stream,
        &ClientMsg::ClientHello {
            capabilities: CAP_ALL,
            prefer_shm: shm_enabled,
        },
    )?;

    let rx = spawn_server_reader(reader_stream, SERVER_MSG_QUEUE_CAPACITY);

    let mut input = InputTracker::default();
    let mut native_cursor = NativeCursorController::default();
    let mut window = create_viewer_window(
        &args.title,
        comp.width as usize,
        comp.height as usize,
        args.max_fps,
        &mut input,
        &mut native_cursor,
    )?;

    let mut last_win_size = window.get_size();
    let mut frame_count = 0u64;
    let mut stat_window_start = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_rtt_us = 0u64;
    let mut dirty = true;

    let loop_result = (|| -> anyhow::Result<()> {
        while window.is_open() {
            let drain_outcome = drain_server_messages(
                &rx,
                MAX_SERVER_MSGS_PER_TICK,
                MAX_SERVER_DRAIN_TIME_PER_TICK,
                |msg| {
                    match msg {
                        ServerMsg::ServerHello { .. } => {}
                        ServerMsg::Resolution {
                            width,
                            height,
                            stride,
                            offset,
                            shm_path,
                        } => {
                            let dims_changed = width != comp.width || height != comp.height;
                            comp.resize(
                                width,
                                height,
                                stride,
                                offset,
                                shm_path.as_deref(),
                                shm_enabled,
                            );
                            if shm_enabled && !comp.is_shm_active() {
                                shm_enabled = false;
                                write_client_msg(
                                    &mut writer_stream,
                                    &ClientMsg::RequestFullFrame { allow_shm: false },
                                )?;
                                fallback_pending = true;
                            }
                            if dims_changed {
                                for msg in input.release_all() {
                                    write_client_msg(&mut writer_stream, &msg)?;
                                }
                                native_cursor.before_window_recreate(&mut window);
                                window = create_viewer_window(
                                    &args.title,
                                    comp.width as usize,
                                    comp.height as usize,
                                    args.max_fps,
                                    &mut input,
                                    &mut native_cursor,
                                )?;
                                last_win_size = window.get_size();
                            }
                            dirty = true;
                        }
                        ServerMsg::FrameBatch { rects, .. } => {
                            if comp.apply_rects(&rects)? {
                                shm_enabled = false;
                                if !fallback_pending {
                                    write_client_msg(
                                        &mut writer_stream,
                                        &ClientMsg::RequestFullFrame { allow_shm: false },
                                    )?;
                                    fallback_pending = true;
                                }
                            } else {
                                fallback_pending = false;
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
                    Ok(())
                },
            )?;
            if drain_outcome == DrainOutcome::Disconnected {
                anyhow::bail!("server connection closed");
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
                let mode = if comp.is_shm_active() {
                    "SHM Zero-Copy"
                } else {
                    "LZ4 Tile"
                };
                window.set_title(&format!(
                    "{} — {}x{} [{}] | {:.1} FPS | RTT {} us",
                    args.title, comp.width, comp.height, mode, fps, last_rtt_us
                ));
                frame_count = 0;
                stat_window_start = Instant::now();
            }

            if detect_window_resize(&mut last_win_size, window.get_size()) {
                dirty = true;
            }
            if dirty {
                if last_win_size.0 > 0 && last_win_size.1 > 0 {
                    window.update_with_buffer(
                        &comp.present_fb,
                        comp.width as usize,
                        comp.height as usize,
                    )?;
                    dirty = false;
                } else {
                    window.update();
                }
                if detect_window_resize(&mut last_win_size, window.get_size()) {
                    dirty = true;
                }
            } else {
                window.update();
                if detect_window_resize(&mut last_win_size, window.get_size()) {
                    if last_win_size.0 > 0 && last_win_size.1 > 0 {
                        window.update_with_buffer(
                            &comp.present_fb,
                            comp.width as usize,
                            comp.height as usize,
                        )?;
                        if detect_window_resize(&mut last_win_size, window.get_size()) {
                            dirty = true;
                        }
                    } else {
                        dirty = true;
                    }
                }
            }
        }
        Ok(())
    })();

    native_cursor.restore(&mut window);
    loop_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastdesk_proto::EncodedRect;
    use fastdesk_proto::RectEncoding;
    use fastdesk_proto::write_server_msg;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[test]
    fn detect_window_resize_tracks_size_transitions() {
        let mut last = (800, 600);
        assert!(!detect_window_resize(&mut last, (800, 600)));
        assert!(detect_window_resize(&mut last, (1024, 768)));
        assert_eq!(last, (1024, 768));
        assert!(!detect_window_resize(&mut last, (1024, 768)));
    }

    #[test]
    fn drain_server_messages_respects_count_and_time_budgets() {
        let (tx, rx) = mpsc::sync_channel(16);
        for seq in 1..=7u64 {
            tx.send(Ok(ServerMsg::Pong {
                client_time_us: seq,
                host_time_us: seq,
            }))
            .unwrap();
        }

        let mut seen = Vec::new();
        let outcome = drain_server_messages(&rx, 3, Duration::from_secs(5), |msg| {
            if let ServerMsg::Pong { client_time_us, .. } = msg {
                seen.push(client_time_us);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome, DrainOutcome::BudgetExhausted);
        assert_eq!(seen, vec![1, 2, 3]);

        // Time budget stops draining before hitting a large message count limit.
        let mut time_seen = Vec::new();
        let outcome = drain_server_messages(&rx, 100, Duration::from_millis(5), |msg| {
            if let ServerMsg::Pong { client_time_us, .. } = msg {
                time_seen.push(client_time_us);
            }
            thread::sleep(Duration::from_millis(6));
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome, DrainOutcome::BudgetExhausted);
        assert_eq!(time_seen, vec![4]);

        // Draining the rest reaches Empty cleanly.
        let mut rest = Vec::new();
        let outcome = drain_server_messages(&rx, 16, Duration::from_secs(5), |msg| {
            if let ServerMsg::Pong { client_time_us, .. } = msg {
                rest.push(client_time_us);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome, DrainOutcome::Empty);
        assert_eq!(rest, vec![5, 6, 7]);

        drop(tx);
        let outcome = drain_server_messages(&rx, 16, Duration::from_secs(5), |_| Ok(())).unwrap();
        assert_eq!(outcome, DrainOutcome::Disconnected);
    }

    #[test]
    fn bounded_server_reader_applies_backpressure_and_prevents_ui_starvation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let sent_count = Arc::new(AtomicUsize::new(0));
        let sent_clone = Arc::clone(&sent_count);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        // Large incompressible payload so kernel TCP send buffers fill quickly under backpressure.
        let mut raw_rect = vec![0u8; 64 * 1024];
        for (i, byte) in raw_rect.iter_mut().enumerate() {
            *byte = ((i * 73 + 19) ^ (i >> 3)) as u8;
        }

        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
            let mut seq = 0u64;
            while !stop_clone.load(Ordering::Relaxed) {
                seq += 1;
                let msg = ServerMsg::FrameBatch {
                    frame_seq: seq,
                    host_time_us: seq,
                    rects: vec![EncodedRect {
                        x: 0,
                        y: 0,
                        w: 128,
                        h: 128,
                        encoding: RectEncoding::RawBgra,
                        data: raw_rect.clone(),
                    }],
                };
                match write_server_msg(&mut sock, &msg) {
                    Ok(()) => {
                        sent_clone.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(fastdesk_proto::ProtoError::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });

        let client = TcpStream::connect(addr).unwrap();
        let queue_cap = 4usize;
        let rx = spawn_server_reader(client, queue_cap);

        // Let the producer blast while consumer is paused; verify bounded backpressure stops growth.
        thread::sleep(Duration::from_millis(80));
        let sent_before_resume = sent_count.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(80));
        let sent_while_paused = sent_count.load(Ordering::Relaxed);
        assert!(
            sent_while_paused - sent_before_resume <= 128,
            "expected TCP backpressure from bounded queue, delta={}",
            sent_while_paused - sent_before_resume
        );

        // Simulate UI loop iterations with slow per-frame compositing (~5ms/msg) and verify
        // input/heartbeat steps still run every iteration without starvation.
        let mut ui_ticks = 0usize;
        let mut total_msgs = 0usize;
        for _ in 0..8 {
            let outcome = drain_server_messages(&rx, 3, Duration::from_millis(8), |_msg| {
                total_msgs += 1;
                thread::sleep(Duration::from_millis(5));
                Ok(())
            })
            .unwrap();
            ui_ticks += 1;
            assert_ne!(outcome, DrainOutcome::Disconnected);
        }
        assert_eq!(ui_ticks, 8);
        assert!(
            total_msgs >= 8 && total_msgs <= 24,
            "unexpected total_msgs under budget: {total_msgs}"
        );

        stop.store(true, Ordering::Relaxed);
        drop(rx);
        server.join().unwrap();
    }

    #[derive(Default)]
    struct MockCursorWindow {
        calls: Vec<bool>,
        show_cursor_count: i32,
    }

    impl CursorVisibilityTarget for MockCursorWindow {
        fn set_native_cursor_visibility(&mut self, visible: bool) {
            self.calls.push(visible);
            if visible {
                self.show_cursor_count += 1;
            } else {
                self.show_cursor_count -= 1;
            }
        }
    }

    #[test]
    fn native_cursor_controller_manages_initial_creation_recreation_and_restore() {
        let mut ctrl = NativeCursorController::default();
        let mut win1 = MockCursorWindow::default();

        // Initial creation hides native cursor once; duplicate calls are idempotent.
        ctrl.on_window_created(&mut win1);
        ctrl.on_window_created(&mut win1);
        assert_eq!(win1.calls, vec![false]);
        assert_eq!(win1.show_cursor_count, -1);

        // Resolution change recreates window: restore old window, hide new window.
        ctrl.before_window_recreate(&mut win1);
        assert_eq!(win1.calls, vec![false, true]);
        assert_eq!(win1.show_cursor_count, 0);

        let mut win2 = MockCursorWindow::default();
        ctrl.on_window_created(&mut win2);
        assert_eq!(win2.calls, vec![false]);
        assert_eq!(win2.show_cursor_count, -1);

        // Exit restores native cursor cleanly (and repeated restore is a no-op).
        ctrl.restore(&mut win2);
        ctrl.restore(&mut win2);
        assert_eq!(win2.calls, vec![false, true]);
        assert_eq!(win2.show_cursor_count, 0);
    }

    #[test]
    fn win32_setcursor_hides_only_for_client_area_of_target_window() {
        let target_hwnd = 0x1234usize;
        let other_hwnd = 0x5678usize;

        // Client area (HTCLIENT = 1) hides when enabled for the target HWND,
        // regardless of the mouse message code in HIWORD(lparam).
        assert!(should_hide_win32_cursor_for_setcursor(
            true,
            target_hwnd,
            target_hwnd,
            WIN32_HTCLIENT as isize
        ));
        assert!(should_hide_win32_cursor_for_setcursor(
            true,
            target_hwnd,
            target_hwnd,
            ((0x0200usize << 16) | WIN32_HTCLIENT as usize) as isize
        ));

        // Non-client controls (title bar, system menu, min/max/close buttons, resize borders,
        // and error hit-test codes) must leave the native cursor visible.
        for non_client_hit in [
            0isize, // HTNOWHERE
            2,      // HTCAPTION
            3,      // HTSYSMENU
            8,      // HTMINBUTTON
            9,      // HTMAXBUTTON
            10,     // HTLEFT
            11,     // HTRIGHT
            12,     // HTTOP
            13,     // HTTOPLEFT
            14,     // HTTOPRIGHT
            15,     // HTBOTTOM
            16,     // HTBOTTOMLEFT
            17,     // HTBOTTOMRIGHT
            20,     // HTCLOSE
            0xfffe, // HTERROR (-2 as u16)
        ] {
            let lparam = ((0x0200usize << 16) | (non_client_hit as usize & 0xffff)) as isize;
            assert!(
                !should_hide_win32_cursor_for_setcursor(true, target_hwnd, target_hwnd, lparam),
                "unexpectedly hid cursor for non-client hit test {non_client_hit}"
            );
        }

        // Disabled, mismatched HWND, or null HWND must not hide.
        assert!(!should_hide_win32_cursor_for_setcursor(
            false,
            target_hwnd,
            target_hwnd,
            WIN32_HTCLIENT as isize
        ));
        assert!(!should_hide_win32_cursor_for_setcursor(
            true,
            target_hwnd,
            other_hwnd,
            WIN32_HTCLIENT as isize
        ));
        assert!(!should_hide_win32_cursor_for_setcursor(
            true,
            0,
            0,
            WIN32_HTCLIENT as isize
        ));
    }
}
