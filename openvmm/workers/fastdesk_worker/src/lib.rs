// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Event-driven low-latency FastDesk display and input worker.
//!
//! Replaces VNC's 30ms polling timer with immediate wakeup on `SYNTHVID_DIRT`
//! and hardware cursor updates, supporting lossless LZ4 dirty-tile streaming
//! over TCP/`AF_HYPERV`/`AF_VSOCK` as well as zero-copy shared memory mode.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Context;
use anyhow::anyhow;
use fastdesk_proto::BTN_WHEEL_DOWN;
use fastdesk_proto::BTN_WHEEL_UP;
use fastdesk_proto::CAP_ALL;
use fastdesk_proto::ClientMsg;
use fastdesk_proto::DirtyTileTracker;
use fastdesk_proto::EncodedRect;
use fastdesk_proto::RectEncoding;
use fastdesk_proto::ServerMsg;
use fastdesk_proto::TileRect;
use fastdesk_proto::decode_client_msg;
use fastdesk_proto::encode_bgra_rect;
use fastdesk_proto::encode_server_msg;
use fastdesk_worker_defs::FASTDESK_WORKER_TCP;
#[cfg(any(windows, target_os = "linux"))]
use fastdesk_worker_defs::FASTDESK_WORKER_VMSOCKET;
use fastdesk_worker_defs::FastDeskParameters;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use input_core::InputData;
use input_core::KeyboardData;
use input_core::MouseData;
use mesh::message::MeshField;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::local::LocalDriver;
use pal_async::local::block_with_io;
use pal_async::socket::Listener;
use pal_async::socket::PolledSocket;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use std::future::Future;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tracing_helpers::AnyhowValueExt;
use video_core::CursorPosition;
use video_core::CursorShape;
use video_core::CursorUpdate;
use video_core::DirtyRect;

/// Event-driven FastDesk display/input worker supporting TCP and `vmsocket`.
pub struct FastDeskWorker<T: Listener> {
    listener: T,
    view: ViewWrapper,
    shm_path: Option<String>,
    input_send: mesh::Sender<InputData>,
    dirty_recv: Option<mesh::Receiver<Vec<DirtyRect>>>,
    cursor_recv: Option<mesh::Receiver<CursorUpdate>>,
    max_clients: usize,
}

impl Worker for FastDeskWorker<TcpListener> {
    type Parameters = FastDeskParameters<TcpListener>;
    type State = FastDeskParameters<TcpListener>;
    const ID: WorkerId<Self::Parameters> = FASTDESK_WORKER_TCP;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl Worker for FastDeskWorker<vmsocket::VmListener> {
    type Parameters = FastDeskParameters<vmsocket::VmListener>;
    type State = FastDeskParameters<vmsocket::VmListener>;
    const ID: WorkerId<Self::Parameters> = FASTDESK_WORKER_VMSOCKET;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        Self::new_inner(params)
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(self, rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.run_inner(rpc_recv)
    }
}

impl<T: 'static + Listener + MeshField + Send> FastDeskWorker<T> {
    fn new_inner(params: FastDeskParameters<T>) -> anyhow::Result<Self> {
        Ok(Self {
            listener: params.listener,
            view: ViewWrapper(
                params
                    .framebuffer
                    .view()
                    .context("failed to map framebuffer")?,
            ),
            shm_path: params.shm_path,
            input_send: params.input_send,
            dirty_recv: params.dirty_recv,
            cursor_recv: params.cursor_recv,
            max_clients: params.max_clients,
        })
    }

    fn run_inner(
        self,
        mut rpc_recv: mesh::Receiver<WorkerRpc<FastDeskParameters<T>>>,
    ) -> anyhow::Result<()> {
        block_with_io(async |driver| {
            tracing::info!(
                address = ?self.listener.local_addr().unwrap(),
                shm_path = ?self.shm_path,
                "FastDesk server listening"
            );

            let listener = PolledSocket::new(&driver, self.listener)?;
            let mut server = MultiClientServer {
                listener,
                view: Arc::new(Mutex::new(self.view)),
                shm_path: self.shm_path,
                input_send: self.input_send,
                dirty_recv: self.dirty_recv,
                cursor_recv: self.cursor_recv,
                latest_cursor_shape: None,
                latest_cursor_pos: None,
                device_dirty_seen: Arc::new(AtomicBool::new(false)),
                client_handles: Vec::new(),
                clients: unicycle::FuturesUnordered::new(),
                next_client_id: 0,
                max_clients: self.max_clients,
                start_time: Instant::now(),
            };

            let rpc = loop {
                let r = futures::select! {
                    r = rpc_recv.recv().fuse() => r,
                    r = server.process(&driver).fuse() => break r.map(|_| None)?,
                };
                match r {
                    Ok(message) => match message {
                        WorkerRpc::Stop => break None,
                        WorkerRpc::Inspect(deferred) => deferred.inspect(&server),
                        WorkerRpc::Restart(response) => break Some(response),
                    },
                    Err(_) => break None,
                }
            };
            if let Some(rpc) = rpc {
                server.abort_all_clients().await;
                let view = Arc::try_unwrap(server.view)
                    .expect("all clients terminated")
                    .into_inner();
                let state = FastDeskParameters {
                    listener: server.listener.into_inner(),
                    framebuffer: view.0.access(),
                    shm_path: server.shm_path,
                    input_send: server.input_send,
                    dirty_recv: server.dirty_recv,
                    cursor_recv: server.cursor_recv,
                    max_clients: server.max_clients,
                };
                rpc.complete(Ok(state));
            }
            Ok(())
        })
    }
}

#[derive(Debug)]
struct ViewWrapper(framebuffer::View);

struct ClientHandle {
    id: u64,
    dirty_send: async_channel::Sender<Arc<Vec<DirtyRect>>>,
    cursor_send: async_channel::Sender<Arc<CursorUpdate>>,
    wakeup_send: async_channel::Sender<()>,
    missed_dirty: Arc<AtomicBool>,
    _abort_send: mesh::OneshotSender<()>,
}

struct MultiClientServer<T: Listener> {
    listener: PolledSocket<T>,
    view: Arc<Mutex<ViewWrapper>>,
    shm_path: Option<String>,
    input_send: mesh::Sender<InputData>,
    dirty_recv: Option<mesh::Receiver<Vec<DirtyRect>>>,
    cursor_recv: Option<mesh::Receiver<CursorUpdate>>,
    latest_cursor_shape: Option<Arc<CursorShape>>,
    latest_cursor_pos: Option<CursorPosition>,
    device_dirty_seen: Arc<AtomicBool>,
    client_handles: Vec<ClientHandle>,
    clients: unicycle::FuturesUnordered<Pin<Box<dyn Future<Output = u64>>>>,
    next_client_id: u64,
    max_clients: usize,
    start_time: Instant,
}

impl<T: Listener> MultiClientServer<T> {
    async fn process(&mut self, driver: &LocalDriver) -> anyhow::Result<()> {
        enum Event<A> {
            Accepted(A),
            ClientDone(u64),
            DirtyRects(Vec<DirtyRect>),
            Cursor(CursorUpdate),
            FallbackTick,
        }

        let mut timer = PolledTimer::new(driver);

        loop {
            let listener = &mut self.listener;
            let clients = &mut self.clients;
            let dirty_recv = &mut self.dirty_recv;
            let cursor_recv = &mut self.cursor_recv;
            let fallback_active =
                !self.device_dirty_seen.load(Ordering::Acquire) && !self.client_handles.is_empty();

            let dirty_fut = async {
                match dirty_recv {
                    Some(recv) => recv.recv().await,
                    None => std::future::pending().await,
                }
            };
            let cursor_fut = async {
                match cursor_recv {
                    Some(recv) => recv.recv().await,
                    None => std::future::pending().await,
                }
            };
            let client_done = async {
                if clients.is_empty() {
                    std::future::pending().await
                } else {
                    clients.select_next_some().await
                }
            };
            let fallback_fut = async {
                if fallback_active {
                    timer.sleep(Duration::from_millis(16)).await;
                } else {
                    std::future::pending().await
                }
            };

            let event = futures::select! {
                accept = listener.accept().fuse() => {
                    let (socket, addr) = accept?;
                    Event::Accepted((socket, addr))
                }
                id = client_done.fuse() => Event::ClientDone(id),
                msg = dirty_fut.fuse() => match msg {
                    Ok(rects) => Event::DirtyRects(rects),
                    Err(_) => {
                        tracing::warn!("device dirty channel closed, enabling fallback diff");
                        self.dirty_recv = None;
                        self.device_dirty_seen.store(false, Ordering::Release);
                        continue;
                    }
                },
                msg = cursor_fut.fuse() => match msg {
                    Ok(update) => Event::Cursor(update),
                    Err(_) => {
                        self.cursor_recv = None;
                        continue;
                    }
                },
                _ = fallback_fut.fuse() => Event::FallbackTick,
            };

            match event {
                Event::Accepted((socket, remote_addr)) => {
                    if self.client_handles.len() >= self.max_clients {
                        tracing::warn!(
                            addr = ?remote_addr,
                            max = self.max_clients,
                            "FastDesk client rejected, limit reached"
                        );
                        continue;
                    }
                    let sock: socket2::Socket = socket.into();
                    let _ = sock.set_tcp_nodelay(true);
                    match PolledSocket::new(driver, sock) {
                        Ok(socket) => self.spawn_client(socket, remote_addr),
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                "failed to register FastDesk client socket"
                            );
                        }
                    }
                }
                Event::ClientDone(id) => {
                    self.client_handles.retain(|c| c.id != id);
                    tracing::info!(
                        id,
                        count = self.client_handles.len(),
                        "FastDesk client disconnected"
                    );
                }
                Event::DirtyRects(mut rects) => {
                    if let Some(recv) = &mut self.dirty_recv {
                        while let Ok(more) = recv.try_recv() {
                            rects.extend(more);
                        }
                    }
                    self.device_dirty_seen.store(true, Ordering::Release);
                    let rects = Arc::new(rects);
                    for c in &self.client_handles {
                        if c.dirty_send.try_send(Arc::clone(&rects)).is_err() {
                            c.missed_dirty.store(true, Ordering::Release);
                        }
                        let _ = c.wakeup_send.try_send(());
                    }
                }
                Event::Cursor(first_update) => {
                    let mut updates = vec![first_update];
                    if let Some(recv) = &mut self.cursor_recv {
                        while let Ok(more) = recv.try_recv() {
                            updates.push(more);
                        }
                    }
                    for update in updates {
                        match &update {
                            CursorUpdate::Position(pos) => {
                                self.latest_cursor_pos = Some(*pos);
                            }
                            CursorUpdate::Shape(shape) => {
                                self.latest_cursor_shape = Some(Arc::new(shape.clone()));
                            }
                        }
                        let arc = Arc::new(update);
                        for c in &self.client_handles {
                            let _ = c.cursor_send.try_send(Arc::clone(&arc));
                            let _ = c.wakeup_send.try_send(());
                        }
                    }
                }
                Event::FallbackTick => {
                    for c in &self.client_handles {
                        let _ = c.wakeup_send.try_send(());
                    }
                }
            }
        }
    }

    fn spawn_client(
        &mut self,
        socket: PolledSocket<socket2::Socket>,
        remote_addr: impl std::fmt::Debug,
    ) {
        let id = self.next_client_id;
        self.next_client_id += 1;
        tracing::info!(
            id,
            addr = ?remote_addr,
            count = self.client_handles.len() + 1,
            "FastDesk client connected"
        );

        let (dirty_send, dirty_recv) = async_channel::bounded(32);
        let (cursor_send, cursor_recv) = async_channel::bounded(64);
        let (wakeup_send, wakeup_recv) = async_channel::bounded(1);
        let missed_dirty = Arc::new(AtomicBool::new(false));
        let (abort_send, abort_recv) = mesh::oneshot();

        self.client_handles.push(ClientHandle {
            id,
            dirty_send,
            cursor_send,
            wakeup_send,
            missed_dirty: missed_dirty.clone(),
            _abort_send: abort_send,
        });

        let ctx = ClientContext {
            socket,
            view: self.view.clone(),
            shm_path: self.shm_path.clone(),
            input_send: self.input_send.clone(),
            dirty_recv,
            cursor_recv,
            wakeup_recv,
            missed_dirty,
            device_dirty_seen: self.device_dirty_seen.clone(),
            initial_cursor_shape: self.latest_cursor_shape.clone(),
            initial_cursor_pos: self.latest_cursor_pos,
            start_time: self.start_time,
        };

        let fut = Box::pin(async move {
            let r = futures::select! {
                r = run_client(ctx).fuse() => r,
                _ = abort_recv.fuse() => Err(anyhow!("FastDesk connection aborted")),
            };
            if let Err(err) = r {
                tracing::debug!(error = err.as_error(), id, "FastDesk client ended");
            }
            id
        });
        self.clients.push(fut);
    }

    async fn abort_all_clients(&mut self) {
        self.client_handles.clear();
        while self.clients.next().await.is_some() {}
    }
}

impl<T: Listener> inspect::Inspect for MultiClientServer<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.display_debug("local_addr", &self.listener.get().local_addr().unwrap());
        resp.field("client_count", self.client_handles.len());
        resp.field("shm_path", self.shm_path.as_deref());
        resp.field(
            "device_dirty_seen",
            self.device_dirty_seen.load(Ordering::Relaxed),
        );
    }
}

struct ClientContext {
    socket: PolledSocket<socket2::Socket>,
    view: Arc<Mutex<ViewWrapper>>,
    shm_path: Option<String>,
    input_send: mesh::Sender<InputData>,
    dirty_recv: async_channel::Receiver<Arc<Vec<DirtyRect>>>,
    cursor_recv: async_channel::Receiver<Arc<CursorUpdate>>,
    wakeup_recv: async_channel::Receiver<()>,
    missed_dirty: Arc<AtomicBool>,
    device_dirty_seen: Arc<AtomicBool>,
    initial_cursor_shape: Option<Arc<CursorShape>>,
    initial_cursor_pos: Option<CursorPosition>,
    start_time: Instant,
}

fn current_display_dims(view: &Arc<Mutex<ViewWrapper>>) -> (u16, u16, u32, usize) {
    let mut guard = view.lock();
    let vram_size = guard.0.len();
    if let Some(fmt) = guard.0.format() {
        let w = (fmt.width.max(1) as usize).min(u16::MAX as usize) as u16;
        let h = (fmt.height.max(1) as usize).min(u16::MAX as usize) as u16;
        let stride = fmt.bytes_per_line.max(w as usize * 4) as u32;
        (w, h, stride, vram_size)
    } else {
        (1024, 768, 1024 * 4, vram_size)
    }
}

fn encode_cursor_shape_msg(shape: &CursorShape) -> ServerMsg {
    ServerMsg::CursorShape {
        width: shape.width,
        height: shape.height,
        hotspot_x: shape.hotspot_x,
        hotspot_y: shape.hotspot_y,
        lz4_bgra: lz4_flex::compress_prepend_size(&shape.argb_pixels),
    }
}

async fn read_client_frame(socket: &mut PolledSocket<socket2::Socket>) -> anyhow::Result<ClientMsg> {
    let mut hdr = [0u8; 5];
    socket.read_exact(&mut hdr).await?;
    let (msg_type, payload_len) = fastdesk_proto::decode_frame_header(&hdr)?;
    let mut payload = vec![0u8; payload_len];
    socket.read_exact(&mut payload).await?;
    Ok(decode_client_msg(msg_type, &payload)?)
}

fn dispatch_client_msg(
    msg: ClientMsg,
    input_send: &mesh::Sender<InputData>,
    force_full: &mut bool,
    force_pixels: &mut bool,
    wakeup_ready: &mut bool,
    pong_out: &mut Option<u64>,
) {
    match msg {
        ClientMsg::ClientHello { .. } => {}
        ClientMsg::KeyEvent { scancode, make } => {
            input_send.send(InputData::Keyboard(KeyboardData {
                code: scancode,
                make,
            }));
        }
        ClientMsg::PointerEvent {
            button_mask,
            x,
            y,
            wheel_delta,
        } => {
            let btns = button_mask & 0x07;
            input_send.send(InputData::Mouse(MouseData {
                button_mask: btns,
                x,
                y,
            }));
            if wheel_delta != 0 {
                let steps = wheel_delta.clamp(-8, 8);
                let wheel_bit = if steps > 0 {
                    BTN_WHEEL_UP
                } else {
                    BTN_WHEEL_DOWN
                };
                for _ in 0..steps.unsigned_abs() {
                    input_send.send(InputData::Mouse(MouseData {
                        button_mask: btns | wheel_bit,
                        x,
                        y,
                    }));
                }
                input_send.send(InputData::Mouse(MouseData {
                    button_mask: btns,
                    x,
                    y,
                }));
            }
        }
        ClientMsg::RequestFullFrame { allow_shm } => {
            *force_full = true;
            *force_pixels = !allow_shm;
            *wakeup_ready = true;
        }
        ClientMsg::Ping { client_time_us } => {
            *pong_out = Some(client_time_us);
        }
    }
}

async fn run_client(mut ctx: ClientContext) -> anyhow::Result<()> {
    let (mut width, mut height, mut stride, vram_size) = current_display_dims(&ctx.view);

    let mut out_buf = Vec::with_capacity(64 * 1024);
    encode_server_msg(
        &ServerMsg::ServerHello {
            capabilities: CAP_ALL,
            width,
            height,
            stride,
            vram_size: vram_size as u32,
            shm_path: ctx.shm_path.clone(),
        },
        &mut out_buf,
    )?;
    ctx.socket.write_all(&out_buf).await?;

    let use_shm = match read_client_frame(&mut ctx.socket).await? {
        ClientMsg::ClientHello { prefer_shm, .. } => prefer_shm && ctx.shm_path.is_some(),
        other => anyhow::bail!("expected ClientHello, got {other:?}"),
    };

    out_buf.clear();
    if let Some(shape) = ctx.initial_cursor_shape.take() {
        encode_server_msg(&encode_cursor_shape_msg(&shape), &mut out_buf)?;
    }
    if let Some(pos) = ctx.initial_cursor_pos.take() {
        encode_server_msg(
            &ServerMsg::CursorPosition {
                visible: pos.is_visible,
                x: pos.x,
                y: pos.y,
            },
            &mut out_buf,
        )?;
    }
    if !out_buf.is_empty() {
        ctx.socket.write_all(&out_buf).await?;
    }

    let mut tracker = DirtyTileTracker::new(width, height);
    let mut merged_rects: Vec<TileRect> = Vec::new();
    let mut raw_scratch: Vec<u8> = Vec::new();
    let mut prev_fb: Vec<u8> = Vec::new();
    let mut cur_fb: Vec<u8> = Vec::new();
    let mut frame_seq = 0u64;
    let mut force_full = true;
    let mut force_pixels = false;
    let mut msg_type = [0u8; 1];

    loop {
        let mut wakeup_ready = force_full;
        if !wakeup_ready {
            let mut read_ready = false;
            futures::select! {
                r = ctx.socket.read(&mut msg_type).fuse() => {
                    match r {
                        Ok(0) => return Ok(()),
                        Ok(_) => read_ready = true,
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(()),
                        Err(e) => return Err(e.into()),
                    }
                }
                w = Box::pin(ctx.wakeup_recv.recv()).fuse() => {
                    if w.is_err() {
                        return Ok(());
                    }
                    wakeup_ready = true;
                }
            }

            if read_ready {
                let mut len_buf = [0u8; 4];
                ctx.socket.read_exact(&mut len_buf).await?;
                let payload_len = u32::from_le_bytes(len_buf) as usize;
                if payload_len > fastdesk_proto::MAX_FRAME_PAYLOAD_BYTES {
                    anyhow::bail!("client frame payload too large: {payload_len}");
                }
                let mut payload = vec![0u8; payload_len];
                ctx.socket.read_exact(&mut payload).await?;
                let msg = decode_client_msg(msg_type[0], &payload)?;
                let mut pong_ts = None;
                dispatch_client_msg(
                    msg,
                    &ctx.input_send,
                    &mut force_full,
                    &mut force_pixels,
                    &mut wakeup_ready,
                    &mut pong_ts,
                );
                if let Some(client_time_us) = pong_ts {
                    out_buf.clear();
                    encode_server_msg(
                        &ServerMsg::Pong {
                            client_time_us,
                            host_time_us: ctx.start_time.elapsed().as_micros() as u64,
                        },
                        &mut out_buf,
                    )?;
                    ctx.socket.write_all(&out_buf).await?;
                }
            }
        }

        if !wakeup_ready {
            continue;
        }

        out_buf.clear();

        // 1. Drain pending cursor updates (coalesce consecutive positions).
        let mut latest_pos = None;
        while let Ok(update) = ctx.cursor_recv.try_recv() {
            match update.as_ref() {
                CursorUpdate::Position(pos) => latest_pos = Some(*pos),
                CursorUpdate::Shape(shape) => {
                    encode_server_msg(&encode_cursor_shape_msg(shape), &mut out_buf)?;
                }
            }
        }
        if let Some(pos) = latest_pos {
            encode_server_msg(
                &ServerMsg::CursorPosition {
                    visible: pos.is_visible,
                    x: pos.x,
                    y: pos.y,
                },
                &mut out_buf,
            )?;
        }

        // 2. Check resolution changes.
        let (new_w, new_h, new_stride, _) = current_display_dims(&ctx.view);
        if new_w != width || new_h != height || new_stride != stride {
            width = new_w;
            height = new_h;
            stride = new_stride;
            tracker.resize(width, height);
            prev_fb.clear();
            cur_fb.clear();
            force_full = true;
            encode_server_msg(
                &ServerMsg::Resolution {
                    width,
                    height,
                    stride,
                    shm_path: ctx.shm_path.clone(),
                },
                &mut out_buf,
            )?;
        }

        // 3. Collect dirty regions.
        tracker.clear();
        let mut saw_rects = false;
        while let Ok(rects) = ctx.dirty_recv.try_recv() {
            saw_rects = true;
            for r in rects.iter() {
                tracker.mark_rect(r.left, r.top, r.right, r.bottom);
            }
        }

        if force_full || ctx.missed_dirty.swap(false, Ordering::AcqRel) {
            tracker.mark_all();
            force_full = false;
        } else if !saw_rects && !ctx.device_dirty_seen.load(Ordering::Acquire) {
            let line_bytes = width as usize * 4;
            let total_bytes = line_bytes * height as usize;
            cur_fb.resize(total_bytes, 0);
            {
                let mut guard = ctx.view.lock();
                for y in 0..height {
                    let row = &mut cur_fb[y as usize * line_bytes..(y as usize + 1) * line_bytes];
                    guard.0.read_line(y, row);
                }
            }
            if prev_fb.len() != total_bytes {
                tracker.mark_all();
            } else {
                tracker.diff_frames(&prev_fb, &cur_fb, line_bytes);
            }
            std::mem::swap(&mut prev_fb, &mut cur_fb);
        } else if ctx.device_dirty_seen.load(Ordering::Acquire) && !prev_fb.is_empty() {
            prev_fb = Vec::new();
            cur_fb = Vec::new();
        }

        // 4. Encode dirty rectangles (SHM zero-copy vs LZ4 dirty-column read).
        if !tracker.is_empty() {
            tracker.merge_into(&mut merged_rects);
            let shm_mode = use_shm && !force_pixels;
            force_pixels = false;

            let mut encoded_rects = Vec::with_capacity(merged_rects.len());
            if shm_mode {
                for r in &merged_rects {
                    encoded_rects.push(EncodedRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        encoding: RectEncoding::ShmDirtyOnly,
                        data: Vec::new(),
                    });
                }
            } else {
                let mut guard = ctx.view.lock();
                for r in &merged_rects {
                    let row_bytes = r.w as usize * 4;
                    raw_scratch.resize(row_bytes * r.h as usize, 0);
                    for dy in 0..r.h {
                        let dst =
                            &mut raw_scratch[dy as usize * row_bytes..(dy as usize + 1) * row_bytes];
                        guard.0.read_rect_row(r.x, r.y + dy, r.w, dst);
                    }
                    let (encoding, data) = encode_bgra_rect(&raw_scratch);
                    encoded_rects.push(EncodedRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        encoding,
                        data,
                    });
                }
            }

            frame_seq += 1;
            encode_server_msg(
                &ServerMsg::FrameBatch {
                    frame_seq,
                    host_time_us: ctx.start_time.elapsed().as_micros() as u64,
                    rects: encoded_rects,
                },
                &mut out_buf,
            )?;
        }

        if !out_buf.is_empty() {
            ctx.socket.write_all(&out_buf).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastdesk_proto::BTN_LEFT;
    use fastdesk_proto::BTN_WHEEL_UP;
    use fastdesk_proto::decode_bgra_rect;
    use fastdesk_proto::read_server_msg;
    use fastdesk_proto::write_client_msg;
    use framebuffer::FRAMEBUFFER_SIZE;
    use sparse_mmap::SparseMapping;
    use sparse_mmap::alloc_shared_memory;
    use sparse_mmap::new_mappable_from_file;
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::thread;
    use std::thread::JoinHandle;
    use video_core::FramebufferFormat;

    struct TestServer {
        addr: SocketAddr,
        vram: SparseMapping,
        input_recv: mesh::Receiver<InputData>,
        dirty_send: mesh::Sender<Vec<DirtyRect>>,
        cursor_send: mesh::Sender<CursorUpdate>,
        stop_send: Option<mesh::OneshotSender<()>>,
        join: Option<JoinHandle<anyhow::Result<()>>>,
    }

    impl TestServer {
        fn stop(mut self) {
            if let Some(stop) = self.stop_send.take() {
                stop.send(());
            }
            if let Some(join) = self.join.take() {
                let res = join.join().unwrap();
                assert!(res.is_ok(), "{res:?}");
            }
        }
    }

    fn start_test_server(
        width: u16,
        height: u16,
        shm_file: Option<&std::path::Path>,
    ) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let (vram, shm_path_str) = if let Some(path) = shm_file {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .unwrap();
            f.set_len(FRAMEBUFFER_SIZE as u64).unwrap();
            let m = new_mappable_from_file(&f, true, false).unwrap();
            (m, Some(path.to_string_lossy().into_owned()))
        } else {
            (
                alloc_shared_memory(FRAMEBUFFER_SIZE, "fastdesk-test").unwrap(),
                None,
            )
        };
        let writer_vram = vram.try_clone().unwrap();
        let (fb, access) = framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).unwrap();
        fb.format_send().send(FramebufferFormat {
            width: width as usize,
            height: height as usize,
            bytes_per_line: width as usize * 4,
            offset: 0,
        });

        let mapping = SparseMapping::new(FRAMEBUFFER_SIZE).unwrap();
        mapping
            .map_file(0, FRAMEBUFFER_SIZE, &writer_vram, 0, true)
            .unwrap();

        let (input_send, input_recv) = mesh::channel();
        let (dirty_send, dirty_recv) = mesh::channel();
        let (cursor_send, cursor_recv) = mesh::channel();
        let (stop_send, stop_recv) = mesh::oneshot();

        let join = thread::spawn(move || {
            block_with_io(async |driver| -> anyhow::Result<()> {
                let mut server = MultiClientServer {
                    listener: PolledSocket::new(&driver, listener)?,
                    view: Arc::new(Mutex::new(ViewWrapper(access.view().unwrap()))),
                    shm_path: shm_path_str,
                    input_send,
                    dirty_recv: Some(dirty_recv),
                    cursor_recv: Some(cursor_recv),
                    latest_cursor_shape: None,
                    latest_cursor_pos: None,
                    device_dirty_seen: Arc::new(AtomicBool::new(false)),
                    client_handles: Vec::new(),
                    clients: unicycle::FuturesUnordered::new(),
                    next_client_id: 0,
                    max_clients: 8,
                    start_time: Instant::now(),
                };
                futures::select! {
                    r = server.process(&driver).fuse() => r,
                    _ = stop_recv.fuse() => Ok(()),
                }
            })
        });

        TestServer {
            addr,
            vram: mapping,
            input_recv,
            dirty_send,
            cursor_send,
            stop_send: Some(stop_send),
            join: Some(join),
        }
    }

    fn connect_client(addr: SocketAddr, prefer_shm: bool) -> (TcpStream, ServerMsg) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let hello = read_server_msg(&mut stream).unwrap();
        write_client_msg(
            &mut stream,
            &ClientMsg::ClientHello {
                capabilities: CAP_ALL,
                prefer_shm,
            },
        )
        .unwrap();
        (stream, hello)
    }

    #[test]
    fn event_driven_dirty_lz4_cursor_and_input() {
        let mut server = start_test_server(64, 64, None);
        let (mut stream, hello) = connect_client(server.addr, false);
        assert!(matches!(
            hello,
            ServerMsg::ServerHello {
                width: 64,
                height: 64,
                shm_path: None,
                ..
            }
        ));

        // Initial full frame.
        let initial = read_server_msg(&mut stream).unwrap();
        match initial {
            ServerMsg::FrameBatch { rects, .. } => {
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].w, 64);
                assert_eq!(rects[0].h, 64);
            }
            other => panic!("expected initial FrameBatch, got {other:?}"),
        }

        // Send a dirty rect + VRAM pixel write; verify immediate wakeup (< 25ms) & exact pixels.
        let px = [0x11u8, 0x22, 0x33, 0xff];
        let row_bytes = 64 * 4;
        for y in 16..32usize {
            for x in 16..32usize {
                server
                    .vram
                    .write_at(y * row_bytes + x * 4, &px)
                    .unwrap();
            }
        }
        let sent_at = Instant::now();
        server.dirty_send.send(vec![DirtyRect {
            left: 16,
            top: 16,
            right: 32,
            bottom: 32,
        }]);

        let update = read_server_msg(&mut stream).unwrap();
        let elapsed = sent_at.elapsed();
        assert!(
            elapsed < Duration::from_millis(25),
            "expected immediate event wakeup, took {elapsed:?}"
        );
        match update {
            ServerMsg::FrameBatch { rects, .. } => {
                assert_eq!(rects.len(), 1);
                assert_eq!((rects[0].x, rects[0].y, rects[0].w, rects[0].h), (16, 16, 16, 16));
                let decoded = decode_bgra_rect(rects[0].encoding, &rects[0].data, 16 * 16 * 4).unwrap();
                assert_eq!(&decoded[0..4], &px);
                assert_eq!(&decoded[1020..1024], &px);
            }
            other => panic!("expected dirty FrameBatch, got {other:?}"),
        }

        // Hardware cursor shape and position broadcast.
        server.cursor_send.send(CursorUpdate::Shape(CursorShape {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 0,
            argb_pixels: vec![0xaa; 16],
        }));
        server.cursor_send.send(CursorUpdate::Position(CursorPosition {
            is_visible: true,
            x: 25,
            y: 30,
        }));

        let mut got_shape = false;
        let mut got_pos = false;
        for _ in 0..2 {
            match read_server_msg(&mut stream).unwrap() {
                ServerMsg::CursorShape {
                    width: 2,
                    height: 2,
                    hotspot_x: 1,
                    hotspot_y: 0,
                    lz4_bgra,
                } => {
                    let pixels = lz4_flex::decompress_size_prepended(&lz4_bgra).unwrap();
                    assert_eq!(pixels, vec![0xaa; 16]);
                    got_shape = true;
                }
                ServerMsg::CursorPosition {
                    visible: true,
                    x: 25,
                    y: 30,
                } => got_pos = true,
                other => panic!("unexpected cursor msg: {other:?}"),
            }
        }
        assert!(got_shape && got_pos);

        // Input events (keyboard scancode + mouse + wheel).
        write_client_msg(
            &mut stream,
            &ClientMsg::KeyEvent {
                scancode: 0x1e,
                make: true,
            },
        )
        .unwrap();
        write_client_msg(
            &mut stream,
            &ClientMsg::PointerEvent {
                button_mask: BTN_LEFT,
                x: 1000,
                y: 2000,
                wheel_delta: 2,
            },
        )
        .unwrap();

        let mut received = Vec::new();
        for _ in 0..5 {
            for _ in 0..100 {
                if let Ok(ev) = server.input_recv.try_recv() {
                    received.push(ev);
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(matches!(
            received[0],
            InputData::Keyboard(KeyboardData {
                code: 0x1e,
                make: true
            })
        ));
        assert!(matches!(
            received[1],
            InputData::Mouse(MouseData {
                button_mask: BTN_LEFT,
                x: 1000,
                y: 2000
            })
        ));
        assert!(matches!(
            received[2],
            InputData::Mouse(MouseData {
                button_mask,
                ..
            }) if button_mask == (BTN_LEFT | BTN_WHEEL_UP)
        ));
        assert!(matches!(
            received[4],
            InputData::Mouse(MouseData {
                button_mask: BTN_LEFT,
                ..
            })
        ));

        drop(stream);
        server.stop();
    }

    #[test]
    fn shm_zerocopy_mode_emits_no_pixel_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let shm_file = dir.path().join("vram.shm");
        let server = start_test_server(64, 64, Some(&shm_file));
        let (mut stream, hello) = connect_client(server.addr, true);

        match hello {
            ServerMsg::ServerHello {
                shm_path: Some(p), ..
            } => assert_eq!(p, shm_file.to_string_lossy()),
            other => panic!("expected shm_path in ServerHello, got {other:?}"),
        }

        // Initial frame uses ShmDirtyOnly with empty data.
        match read_server_msg(&mut stream).unwrap() {
            ServerMsg::FrameBatch { rects, .. } => {
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].encoding, RectEncoding::ShmDirtyOnly);
                assert!(rects[0].data.is_empty());
            }
            other => panic!("expected ShmDirtyOnly FrameBatch, got {other:?}"),
        }

        // Subsequent dirty rect emits ShmDirtyOnly and pixel write is visible in shm_file.
        let px = [0xde, 0xad, 0xbe, 0xff];
        server.vram.write_at((4 * 64 + 4) * 4, &px).unwrap();
        server.dirty_send.send(vec![DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]);
        match read_server_msg(&mut stream).unwrap() {
            ServerMsg::FrameBatch { rects, .. } => {
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].encoding, RectEncoding::ShmDirtyOnly);
                assert!(rects[0].data.is_empty());
            }
            other => panic!("expected ShmDirtyOnly FrameBatch, got {other:?}"),
        }
        let disk_bytes = std::fs::read(&shm_file).unwrap();
        let off = (4 * 64 + 4) * 4;
        assert_eq!(&disk_bytes[off..off + 4], &px);

        drop(stream);
        server.stop();
    }
}
