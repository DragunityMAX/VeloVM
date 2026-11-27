// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-platform userspace L2 virtual switch socket/pipe backend.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

pub mod resolver;

use async_trait::async_trait;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use inspect::InspectMut;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

/// Minimum Ethernet frame length (6-byte dst MAC + 6-byte src MAC + 2-byte ethertype).
pub const MIN_L2_FRAME_LEN: usize = 14;
/// Maximum Ethernet frame length supported over the L2 framing protocol (supports jumbo frames).
pub const MAX_L2_FRAME_LEN: usize = 16 * 1024;
const MAX_TX_QUEUE_FRAMES: usize = 256;
const MAX_RX_PACKETS_PER_POLL: usize = 64;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to connect to L2 virtual switch endpoint '{path}'")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to register L2 socket with async driver")]
    RegisterDriver(#[source] std::io::Error),
}

enum RawL2Stream {
    #[cfg(unix)]
    Unix(unix_socket::UnixStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

impl RawL2Stream {
    fn connect(path: &Path) -> Result<Self, Error> {
        #[cfg(unix)]
        {
            let stream = unix_socket::UnixStream::connect(path).map_err(|source| Error::Connect {
                path: path.display().to_string(),
                source,
            })?;
            Ok(Self::Unix(stream))
        }
        #[cfg(windows)]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|source| Error::Connect {
                    path: path.display().to_string(),
                    source,
                })?;
            Ok(Self::Pipe(file))
        }
    }

    fn into_polled(self, driver: &dyn Driver) -> Result<Box<dyn AsyncIoStream>, Error> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => {
                let polled = pal_async::socket::PolledSocket::new(driver, stream)
                    .map_err(Error::RegisterDriver)?;
                Ok(Box::new(polled))
            }
            #[cfg(windows)]
            Self::Pipe(file) => {
                let polled = pal_async::pipe::PolledPipe::new(driver, file)
                    .map_err(Error::RegisterDriver)?;
                Ok(Box::new(polled))
            }
        }
    }
}

trait AsyncIoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + ?Sized> AsyncIoStream for T {}

struct EndpointSlot {
    path: Option<PathBuf>,
    stream: Option<RawL2Stream>,
    active_queue: bool,
}

/// Network endpoint connected to a daemon-hosted userspace L2 virtual switch.
pub struct L2SocketEndpoint {
    slot: Arc<Mutex<EndpointSlot>>,
}

impl L2SocketEndpoint {
    /// Connects to the userspace L2 switch socket (Unix) or named pipe (Windows).
    pub fn connect(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let stream = RawL2Stream::connect(&path)?;
        Ok(Self {
            slot: Arc::new(Mutex::new(EndpointSlot {
                path: Some(path),
                stream: Some(stream),
                active_queue: false,
            })),
        })
    }

    #[cfg(unix)]
    pub fn from_unix_stream(stream: unix_socket::UnixStream) -> Self {
        Self {
            slot: Arc::new(Mutex::new(EndpointSlot {
                path: None,
                stream: Some(RawL2Stream::Unix(stream)),
                active_queue: false,
            })),
        }
    }
}

impl InspectMut for L2SocketEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[async_trait]
impl Endpoint for L2SocketEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "l2_socket"
    }

    async fn get_queues(
        &mut self,
        mut config: Vec<QueueConfig>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.drain(..).next().unwrap();

        let (raw_stream, slot) = {
            let mut guard = self.slot.lock();
            assert!(!guard.active_queue, "L2 socket queue is already active");
            let stream = match guard.stream.take() {
                Some(s) => s,
                None => {
                    let path = guard
                        .path
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("L2 socket stream has no reconnect path"))?;
                    RawL2Stream::connect(path)?
                }
            };
            guard.active_queue = true;
            (stream, self.slot.clone())
        };

        let polled = match raw_stream.into_polled(config.driver.as_ref()) {
            Ok(p) => p,
            Err(err) => {
                self.slot.lock().active_queue = false;
                return Err(err.into());
            }
        };

        queues.push(Box::new(L2SocketQueue::new(polled, Some(slot))));
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(
            !self.slot.lock().active_queue,
            "L2 socket queue has not been dropped"
        );
    }

    fn is_ordered(&self) -> bool {
        true
    }
}

enum RxReadState {
    Header { buf: [u8; 2], read: usize },
    Payload { len: usize, read: usize },
}

struct L2SocketQueue {
    slot: Option<Arc<Mutex<EndpointSlot>>>,
    stream: Option<Box<dyn AsyncIoStream>>,
    rx_free: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
    rx_state: RxReadState,
    rx_buf: Vec<u8>,
    tx_queue: VecDeque<(TxId, Vec<u8>)>,
    tx_offset: usize,
    tx_ready: VecDeque<TxId>,
}

impl Drop for L2SocketQueue {
    fn drop(&mut self) {
        if let Some(slot) = &self.slot {
            slot.lock().active_queue = false;
        }
    }
}

impl InspectMut for L2SocketQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl L2SocketQueue {
    fn new(stream: Box<dyn AsyncIoStream>, slot: Option<Arc<Mutex<EndpointSlot>>>) -> Self {
        Self {
            slot,
            stream: Some(stream),
            rx_free: VecDeque::new(),
            rx_ready: VecDeque::new(),
            rx_state: RxReadState::Header {
                buf: [0; 2],
                read: 0,
            },
            rx_buf: vec![0; MAX_L2_FRAME_LEN],
            tx_queue: VecDeque::new(),
            tx_offset: 0,
            tx_ready: VecDeque::new(),
        }
    }

    fn close_stream(&mut self) {
        self.stream = None;
        while let Some((id, _)) = self.tx_queue.pop_front() {
            self.tx_ready.push_back(id);
        }
        self.tx_offset = 0;
    }

    fn poll_flush_tx(&mut self, cx: &mut Context<'_>) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        while let Some((_, frame)) = self.tx_queue.front() {
            match Pin::new(stream.as_mut()).poll_write(cx, &frame[self.tx_offset..]) {
                Poll::Ready(Ok(0)) => {
                    tracing::warn!("L2 switch stream closed during TX write");
                    self.close_stream();
                    return;
                }
                Poll::Ready(Ok(n)) => {
                    self.tx_offset += n;
                    if self.tx_offset >= frame.len() {
                        let (id, _) = self.tx_queue.pop_front().unwrap();
                        self.tx_offset = 0;
                        self.tx_ready.push_back(id);
                    }
                }
                Poll::Ready(Err(err)) => {
                    tracing::warn!(error = &err as &dyn std::error::Error, "L2 switch TX write error");
                    self.close_stream();
                    return;
                }
                Poll::Pending => return,
            }
        }
    }

    fn poll_read_rx(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) {
        let mut packets_processed = 0;
        while !self.rx_free.is_empty() {
            if packets_processed >= MAX_RX_PACKETS_PER_POLL {
                cx.waker().wake_by_ref();
                break;
            }
            let Some(stream) = self.stream.as_mut() else {
                break;
            };

            match &mut self.rx_state {
                RxReadState::Header { buf, read } => {
                    match Pin::new(stream.as_mut()).poll_read(cx, &mut buf[*read..]) {
                        Poll::Ready(Ok(0)) => {
                            self.close_stream();
                            break;
                        }
                        Poll::Ready(Ok(n)) => {
                            *read += n;
                            if *read == 2 {
                                let len = u16::from_be_bytes(*buf) as usize;
                                if !(MIN_L2_FRAME_LEN..=MAX_L2_FRAME_LEN).contains(&len) {
                                    tracing::warn!(len, "invalid L2 frame length received");
                                    self.close_stream();
                                    break;
                                }
                                self.rx_state = RxReadState::Payload { len, read: 0 };
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            tracing::warn!(
                                error = &err as &dyn std::error::Error,
                                "L2 switch RX header read error"
                            );
                            self.close_stream();
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
                RxReadState::Payload { len, read } => {
                    let target_len = *len;
                    match Pin::new(stream.as_mut())
                        .poll_read(cx, &mut self.rx_buf[*read..target_len])
                    {
                        Poll::Ready(Ok(0)) => {
                            self.close_stream();
                            break;
                        }
                        Poll::Ready(Ok(n)) => {
                            *read += n;
                            if *read == target_len {
                                let rx = self.rx_free.pop_front().expect("checked non-empty");
                                let capacity = pool.capacity(rx) as usize;
                                if target_len <= capacity {
                                    pool.write_packet(
                                        rx,
                                        &RxMetadata {
                                            offset: 0,
                                            len: target_len,
                                            ..Default::default()
                                        },
                                        &self.rx_buf[..target_len],
                                    );
                                    self.rx_ready.push_back(rx);
                                } else {
                                    // Re-queue RX buffer if too small for frame.
                                    self.rx_free.push_front(rx);
                                    tracing::warn!(
                                        frame_len = target_len,
                                        capacity,
                                        "dropping oversized L2 frame for guest RX buffer"
                                    );
                                }
                                self.rx_state = RxReadState::Header {
                                    buf: [0; 2],
                                    read: 0,
                                };
                                packets_processed += 1;
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            tracing::warn!(
                                error = &err as &dyn std::error::Error,
                                "L2 switch RX payload read error"
                            );
                            self.close_stream();
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
            }
        }
    }
}

impl Queue for L2SocketQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) -> Poll<()> {
        self.poll_flush_tx(cx);
        self.poll_read_rx(cx, pool);

        if !self.tx_ready.is_empty() || !self.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, _pool: &mut dyn BufferAccess, done: &[RxId]) {
        self.rx_free.extend(done);
    }

    fn rx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize> {
        let n = std::cmp::min(self.rx_ready.len(), packets.len());
        for (dst, id) in packets[..n].iter_mut().zip(self.rx_ready.drain(..n)) {
            *dst = id;
        }
        Ok(n)
    }

    fn tx_avail(
        &mut self,
        pool: &mut dyn BufferAccess,
        mut segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        let total_segments = segments.len();
        let mut consumed = 0;
        while !segments.is_empty() && self.tx_queue.len() < MAX_TX_QUEUE_FRAMES {
            let (meta, this_segs, _rest) = next_packet(segments);
            let seg_count = this_segs.len();
            let frame = linearize(pool, &mut segments)?;
            consumed += seg_count;

            if self.stream.is_none()
                || !(MIN_L2_FRAME_LEN..=MAX_L2_FRAME_LEN).contains(&frame.len())
            {
                self.tx_ready.push_back(meta.id);
                continue;
            }

            let len_u16 = frame.len() as u16;
            let mut framed = Vec::with_capacity(2 + frame.len());
            framed.extend_from_slice(&len_u16.to_be_bytes());
            framed.extend_from_slice(&frame);
            self.tx_queue.push_back((meta.id, framed));
        }
        debug_assert!(consumed <= total_segments);
        Ok((false, consumed))
    }

    fn tx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        done: &mut [TxId],
    ) -> Result<usize, TxError> {
        let n = std::cmp::min(self.tx_ready.len(), done.len());
        for (dst, id) in done[..n].iter_mut().zip(self.tx_ready.drain(..n)) {
            *dst = id;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use guestmem::GuestMemory;
    use net_backend::RxBufferSegment;
    use net_backend::TxMetadata;
    use net_backend::TxSegmentType;
    use pal_async::DefaultDriver;
    use pal_async::pipe::PolledPipe;
    use pal_async_test::async_test;
    use std::future::poll_fn;

    struct TestPool {
        mem: GuestMemory,
        bufs: Vec<Vec<u8>>,
    }

    impl TestPool {
        fn new(count: usize, buf_size: usize) -> Self {
            Self {
                mem: GuestMemory::allocate(count * buf_size),
                bufs: vec![vec![0; buf_size]; count],
            }
        }
    }

    impl BufferAccess for TestPool {
        fn guest_memory(&self) -> &GuestMemory {
            &self.mem
        }

        fn write_data(&mut self, id: RxId, data: &[u8]) {
            self.bufs[id.0 as usize][..data.len()].copy_from_slice(data);
        }

        fn push_guest_addresses(&self, _id: RxId, _buf: &mut Vec<RxBufferSegment>) {}

        fn capacity(&self, id: RxId) -> u32 {
            self.bufs[id.0 as usize].len() as u32
        }

        fn write_header(&mut self, _id: RxId, _metadata: &RxMetadata) {}
    }

    struct BiPipe {
        rx: PolledPipe,
        tx: PolledPipe,
    }

    impl AsyncRead for BiPipe {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.rx).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for BiPipe {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.tx).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.tx).poll_flush(cx)
        }

        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.tx).poll_close(cx)
        }
    }

    #[async_test]
    async fn queue_exchanges_length_prefixed_ethernet_frames(driver: DefaultDriver) {
        let (hub_rx, q_tx) = PolledPipe::pair(&driver).unwrap();
        let (q_rx, hub_tx) = PolledPipe::pair(&driver).unwrap();
        let q_pipe = BiPipe { rx: q_rx, tx: q_tx };
        let mut hub_pipe = BiPipe { rx: hub_rx, tx: hub_tx };
        let mut queue = L2SocketQueue::new(Box::new(q_pipe), None);
        let mut pool = TestPool::new(4, 2048);

        // Write a guest TX frame into guest memory and submit via tx_avail.
        let tx_frame = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // dst
            0x00, 0x15, 0x5d, 0x01, 0x02, 0x03, // src
            0x08, 0x06, // ARP
            1, 2, 3, 4,
        ];
        pool.mem.write_at(0, &tx_frame).unwrap();
        let seg = TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(7),
                len: tx_frame.len() as u32,
                segment_count: 1,
                ..Default::default()
            }),
            gpa: 0,
            len: tx_frame.len() as u32,
        };
        let (sync_done, consumed) = queue.tx_avail(&mut pool, &[seg]).unwrap();
        assert!(!sync_done);
        assert_eq!(consumed, 1);

        // Post RX buffer id 0.
        queue.rx_avail(&mut pool, &[RxId(0)]);

        // Send a partial frame from the hub first (1 byte header, then rest) to test partial read handling.
        let rx_frame = [
            0x00, 0x15, 0x5d, 0x01, 0x02, 0x03, // dst
            0x00, 0x15, 0x5d, 0xaa, 0xbb, 0xcc, // src
            0x08, 0x00, // IPv4
            9, 8, 7, 6, 5, 4,
        ];
        let rx_len_be = (rx_frame.len() as u16).to_be_bytes();
        hub_pipe.write_all(&rx_len_be[..1]).await.unwrap();

        // Poll queue until TX completes.
        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
        let mut tx_done = [TxId(0); 2];
        assert_eq!(queue.tx_poll(&mut pool, &mut tx_done).unwrap(), 1);
        assert_eq!(tx_done[0].0, 7);

        // Verify hub received exact length-prefixed TX frame.
        let mut hdr = [0u8; 2];
        hub_pipe.read_exact(&mut hdr).await.unwrap();
        assert_eq!(u16::from_be_bytes(hdr) as usize, tx_frame.len());
        let mut got_tx = vec![0u8; tx_frame.len()];
        hub_pipe.read_exact(&mut got_tx).await.unwrap();
        assert_eq!(got_tx, tx_frame);

        // Finish sending RX frame from hub and verify guest receives it.
        hub_pipe.write_all(&rx_len_be[1..]).await.unwrap();
        hub_pipe.write_all(&rx_frame).await.unwrap();

        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
        let mut rx_done = [RxId(99); 2];
        assert_eq!(queue.rx_poll(&mut pool, &mut rx_done).unwrap(), 1);
        assert_eq!(rx_done[0].0, 0);
        assert_eq!(&pool.bufs[0][..rx_frame.len()], &rx_frame);
    }
}
