use crate::vmservice;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use prost::Message;
use socket2::Domain;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const MESSAGE_TYPE_REQUEST: u8 = 1;
const MESSAGE_TYPE_RESPONSE: u8 = 2;
const MAX_MESSAGE_SIZE: usize = 0x00ff_ffff;
const SERVICE_VM: &str = "vmservice.VM";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, prost::Message)]
struct WireRequest {
    #[prost(string, tag = "1")]
    service: String,
    #[prost(string, tag = "2")]
    method: String,
    #[prost(bytes = "vec", tag = "3")]
    payload: Vec<u8>,
    #[prost(uint64, tag = "4")]
    timeout_nano: u64,
    #[prost(message, repeated, tag = "5")]
    metadata: Vec<WireKeyValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct WireKeyValue {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TtrpcStatus {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(message, repeated, tag = "3")]
    pub details: Vec<prost_types::Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct WireResponse {
    #[prost(oneof = "wire_response::Result", tags = "1, 2")]
    result: Option<wire_response::Result>,
}

mod wire_response {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Status(super::TtrpcStatus),
        #[prost(bytes, tag = "2")]
        Payload(Vec<u8>),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("ttrpc error (code {code}): {message}")]
pub struct TtrpcRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitVmOutcome {
    Exited,
    Cancelled,
}

struct CancelSocketGuard {
    cancelled: Arc<AtomicBool>,
    active_sock: Arc<Mutex<Option<Arc<Socket>>>>,
    disarmed: bool,
}

impl CancelSocketGuard {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            active_sock: Arc::new(Mutex::new(None)),
            disarmed: false,
        }
    }

    fn trigger(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(sock) = self.active_sock.lock().unwrap().as_ref() {
            let _ = sock.shutdown(Shutdown::Both);
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for CancelSocketGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.trigger();
        }
    }
}

async fn wait_for_cancel(cancel_rx: &mut tokio::sync::watch::Receiver<bool>) {
    while !*cancel_rx.borrow_and_update() {
        if cancel_rx.changed().await.is_err() {
            break;
        }
    }
}

#[derive(Debug, Clone)]
pub struct TtrpcClient {
    socket_path: PathBuf,
}

impl TtrpcClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let _ = self
            .properties_vm(vmservice::PropertiesVmRequest { types: vec![] })
            .await?;
        Ok(())
    }

    pub async fn create_vm(&self, request: vmservice::CreateVmRequest) -> anyhow::Result<()> {
        let _: () = self.call_unary("CreateVM", request, Some(DEFAULT_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn teardown_vm(&self) -> anyhow::Result<()> {
        let _: () = self.call_unary("TeardownVM", (), Some(DEFAULT_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn pause_vm(&self) -> anyhow::Result<()> {
        let _: () = self.call_unary("PauseVM", (), Some(DEFAULT_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn resume_vm(&self) -> anyhow::Result<()> {
        let _: () = self.call_unary("ResumeVM", (), Some(DEFAULT_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn reset_vm(&self) -> anyhow::Result<()> {
        let _: () = self.call_unary("ResetVM", (), Some(DEFAULT_TIMEOUT)).await?;
        Ok(())
    }

    pub async fn shutdown_vm(&self, request: vmservice::ShutdownVmRequest) -> anyhow::Result<()> {
        let _: () = self
            .call_unary("ShutdownVM", request, Some(Duration::from_secs(15)))
            .await?;
        Ok(())
    }

    pub async fn wait_vm(&self) -> anyhow::Result<()> {
        let _: () = self.call_unary("WaitVM", (), None).await?;
        Ok(())
    }

    pub async fn wait_vm_cancellable(
        &self,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<WaitVmOutcome> {
        if *cancel_rx.borrow_and_update() {
            return Ok(WaitVmOutcome::Cancelled);
        }

        let socket_path = self.socket_path.clone();
        let mut guard = CancelSocketGuard::new();
        let cancelled = Arc::clone(&guard.cancelled);
        let active_sock = Arc::clone(&guard.active_sock);

        let mut blocking = tokio::task::spawn_blocking(move || {
            call_blocking_cancellable(
                &socket_path,
                SERVICE_VM,
                "WaitVM",
                &[],
                None,
                &cancelled,
                &active_sock,
            )
        });

        tokio::select! {
            res = &mut blocking => {
                guard.disarm();
                let _ = res.context("ttrpc WaitVM blocking task failed")??;
                Ok(WaitVmOutcome::Exited)
            }
            _ = wait_for_cancel(&mut cancel_rx) => {
                guard.trigger();
                let _ = blocking.await;
                guard.disarm();
                Ok(WaitVmOutcome::Cancelled)
            }
        }
    }

    pub async fn properties_vm(
        &self,
        request: vmservice::PropertiesVmRequest,
    ) -> anyhow::Result<vmservice::PropertiesVmResponse> {
        self.call_unary("PropertiesVM", request, Some(DEFAULT_TIMEOUT))
            .await
    }

    pub async fn capabilities_vm(&self) -> anyhow::Result<vmservice::CapabilitiesVmResponse> {
        self.call_unary("CapabilitiesVM", (), Some(DEFAULT_TIMEOUT))
            .await
    }

    pub async fn modify_resource(
        &self,
        request: vmservice::ModifyResourceRequest,
    ) -> anyhow::Result<vmservice::ModifyResourceResponse> {
        self.call_unary("ModifyResource", request, Some(DEFAULT_TIMEOUT))
            .await
    }

    pub async fn quit(&self) -> anyhow::Result<()> {
        let _: () = self
            .call_unary("Quit", (), Some(Duration::from_secs(5)))
            .await?;
        Ok(())
    }

    pub fn properties_vm_blocking(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<vmservice::PropertiesVmResponse> {
        let req_bytes = vmservice::PropertiesVmRequest { types: vec![] }.encode_to_vec();
        let resp_bytes = call_blocking(
            &self.socket_path,
            SERVICE_VM,
            "PropertiesVM",
            &req_bytes,
            Some(timeout),
        )?;
        vmservice::PropertiesVmResponse::decode(resp_bytes.as_slice())
            .context("decoding ttrpc PropertiesVM response")
    }

    pub fn teardown_and_quit_blocking(&self, timeout: Duration) -> anyhow::Result<()> {
        let _ = call_blocking(
            &self.socket_path,
            SERVICE_VM,
            "TeardownVM",
            &[],
            Some(timeout),
        );
        let _ = call_blocking(&self.socket_path, SERVICE_VM, "Quit", &[], Some(timeout))?;
        Ok(())
    }

    pub fn is_socket_accepting(&self) -> bool {
        let Ok(sock) = Socket::new(Domain::UNIX, Type::STREAM, None) else {
            return false;
        };
        let Ok(addr) = SockAddr::unix(&self.socket_path) else {
            return false;
        };
        sock.connect(&addr).is_ok()
    }

    async fn call_unary<Req: Message, Resp: Message + Default>(
        &self,
        method: &'static str,
        req: Req,
        timeout: Option<Duration>,
    ) -> anyhow::Result<Resp> {
        let socket_path = self.socket_path.clone();
        let req_bytes = req.encode_to_vec();
        let mut guard = CancelSocketGuard::new();
        let cancelled = Arc::clone(&guard.cancelled);
        let active_sock = Arc::clone(&guard.active_sock);
        let resp_bytes = tokio::task::spawn_blocking(move || {
            call_blocking_cancellable(
                &socket_path,
                SERVICE_VM,
                method,
                &req_bytes,
                timeout,
                &cancelled,
                &active_sock,
            )
        })
        .await
        .context("ttrpc blocking task failed")??;
        guard.disarm();
        Resp::decode(resp_bytes.as_slice()).context("decoding ttrpc response message")
    }
}

pub fn encode_frame(stream_id: u32, message_type: u8, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    if payload.len() > MAX_MESSAGE_SIZE {
        bail!(
            "ttrpc payload length {} exceeds maximum {MAX_MESSAGE_SIZE}",
            payload.len()
        );
    }
    let len = u32::try_from(payload.len()).context("payload length overflow")?;
    let mut out = Vec::with_capacity(10 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.push(message_type);
    out.push(0);
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn read_frame(reader: &mut impl Read) -> anyhow::Result<(u32, u8, Vec<u8>)> {
    let mut header = [0u8; 10];
    reader
        .read_exact(&mut header)
        .context("reading ttrpc frame header")?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let stream_id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let message_type = header[8];
    if len > MAX_MESSAGE_SIZE {
        bail!("ttrpc frame length {len} exceeds maximum {MAX_MESSAGE_SIZE}");
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .context("reading ttrpc frame payload")?;
    Ok((stream_id, message_type, payload))
}

fn read_exact_cancellable(
    sock: &Socket,
    buf: &mut [u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    const POLL_SLICE: Duration = Duration::from_millis(200);
    let mut offset = 0usize;
    while offset < buf.len() {
        if cancelled.load(Ordering::Acquire) {
            bail!("ttrpc call cancelled");
        }
        let slice = if let Some(dl) = deadline {
            let now = Instant::now();
            if now >= dl {
                bail!("ttrpc call timed out");
            }
            (dl - now).min(POLL_SLICE)
        } else {
            POLL_SLICE
        };
        let _ = sock.set_read_timeout(Some(slice));
        match (&*sock).read(&mut buf[offset..]) {
            Ok(0) => {
                if cancelled.load(Ordering::Acquire) {
                    bail!("ttrpc call cancelled");
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF reading ttrpc frame",
                )
                .into());
            }
            Ok(n) => offset += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    bail!("ttrpc call cancelled");
                }
                if let Some(dl) = deadline
                    && Instant::now() >= dl
                {
                    bail!("ttrpc call timed out");
                }
                continue;
            }
            Err(err) => {
                if cancelled.load(Ordering::Acquire) {
                    bail!("ttrpc call cancelled");
                }
                return Err(err.into());
            }
        }
    }
    Ok(())
}

fn read_frame_cancellable(
    sock: &Socket,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> anyhow::Result<(u32, u8, Vec<u8>)> {
    let mut header = [0u8; 10];
    read_exact_cancellable(sock, &mut header, cancelled, deadline)
        .context("reading ttrpc frame header")?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let stream_id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let message_type = header[8];
    if len > MAX_MESSAGE_SIZE {
        bail!("ttrpc frame length {len} exceeds maximum {MAX_MESSAGE_SIZE}");
    }
    let mut payload = vec![0u8; len];
    read_exact_cancellable(sock, &mut payload, cancelled, deadline)
        .context("reading ttrpc frame payload")?;
    Ok((stream_id, message_type, payload))
}

fn call_blocking(
    socket_path: &Path,
    service: &str,
    method: &str,
    payload: &[u8],
    timeout: Option<Duration>,
) -> anyhow::Result<Vec<u8>> {
    let cancelled = AtomicBool::new(false);
    let active_sock = Mutex::new(None);
    call_blocking_cancellable(
        socket_path,
        service,
        method,
        payload,
        timeout,
        &cancelled,
        &active_sock,
    )
}

fn call_blocking_cancellable(
    socket_path: &Path,
    service: &str,
    method: &str,
    payload: &[u8],
    timeout: Option<Duration>,
    cancelled: &AtomicBool,
    active_sock: &Mutex<Option<Arc<Socket>>>,
) -> anyhow::Result<Vec<u8>> {
    if cancelled.load(Ordering::Acquire) {
        bail!("ttrpc call cancelled");
    }
    let deadline = timeout.map(|t| Instant::now() + t);

    let sock = Socket::new(Domain::UNIX, Type::STREAM, None)
        .context("creating AF_UNIX socket for ttrpc")?;
    let addr = SockAddr::unix(socket_path)
        .with_context(|| format!("invalid AF_UNIX path {}", socket_path.display()))?;
    sock.connect(&addr)
        .with_context(|| format!("connecting to OpenVMM RPC socket {}", socket_path.display()))?;
    let _ = sock.set_write_timeout(timeout.or(Some(DEFAULT_TIMEOUT)));

    let sock = Arc::new(sock);
    {
        let mut slot = active_sock.lock().unwrap();
        if cancelled.load(Ordering::Acquire) {
            let _ = sock.shutdown(Shutdown::Both);
            bail!("ttrpc call cancelled");
        }
        *slot = Some(Arc::clone(&sock));
    }

    struct ClearActive<'a>(&'a Mutex<Option<Arc<Socket>>>);
    impl Drop for ClearActive<'_> {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = None;
        }
    }
    let _clear = ClearActive(active_sock);

    let wire_req = WireRequest {
        service: service.to_string(),
        method: method.to_string(),
        payload: payload.to_vec(),
        timeout_nano: timeout
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0),
        metadata: Vec::new(),
    };
    let req_envelope = wire_req.encode_to_vec();
    let frame = encode_frame(1, MESSAGE_TYPE_REQUEST, &req_envelope)?;
    (&*sock).write_all(&frame).context("writing ttrpc request")?;
    (&*sock).flush().context("flushing ttrpc request")?;

    let (stream_id, message_type, resp_envelope) =
        read_frame_cancellable(&sock, cancelled, deadline)?;
    if stream_id != 1 {
        bail!("unexpected ttrpc response stream_id {stream_id}");
    }
    if message_type != MESSAGE_TYPE_RESPONSE {
        bail!("unexpected ttrpc response message type {message_type}");
    }

    let resp = WireResponse::decode(resp_envelope.as_slice())
        .context("decoding ttrpc response envelope")?;
    match resp.result {
        Some(wire_response::Result::Payload(bytes)) => Ok(bytes),
        Some(wire_response::Result::Status(status)) => Err(anyhow!(TtrpcRpcError {
            code: status.code,
            message: status.message,
        })),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[tokio::test]
    async fn ttrpc_roundtrip_success_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&SockAddr::unix(&sock_path).unwrap()).unwrap();
        listener.listen(8).unwrap();

        let server = thread::spawn(move || {
            for idx in 0..2 {
                let (mut conn, _) = listener.accept().unwrap();
                let (stream_id, msg_type, bytes) = read_frame(&mut conn).unwrap();
                assert_eq!(stream_id, 1);
                assert_eq!(msg_type, MESSAGE_TYPE_REQUEST);
                let req = WireRequest::decode(bytes.as_slice()).unwrap();
                assert_eq!(req.service, SERVICE_VM);

                let resp = if idx == 0 {
                    assert_eq!(req.method, "PropertiesVM");
                    let payload = vmservice::PropertiesVmResponse {
                        state: vmservice::VmState::Running as i32,
                        fastdesk_listen_addr: Some("tcp://127.0.0.1:5901".into()),
                        ..Default::default()
                    }
                    .encode_to_vec();
                    WireResponse {
                        result: Some(wire_response::Result::Payload(payload)),
                    }
                } else {
                    assert_eq!(req.method, "ResetVM");
                    WireResponse {
                        result: Some(wire_response::Result::Status(TtrpcStatus {
                            code: 9,
                            message: "cannot reset".into(),
                            details: vec![],
                        })),
                    }
                };
                let frame = encode_frame(1, MESSAGE_TYPE_RESPONSE, &resp.encode_to_vec()).unwrap();
                conn.write_all(&frame).unwrap();
                conn.flush().unwrap();
            }
        });

        let client = TtrpcClient::new(&sock_path);
        let props = client
            .properties_vm(vmservice::PropertiesVmRequest { types: vec![] })
            .await
            .unwrap();
        assert_eq!(props.state, vmservice::VmState::Running as i32);
        assert_eq!(
            props.fastdesk_listen_addr.as_deref(),
            Some("tcp://127.0.0.1:5901")
        );

        let err = client.reset_vm().await.unwrap_err();
        let rpc_err = err.downcast_ref::<TtrpcRpcError>().unwrap();
        assert_eq!(rpc_err.code, 9);
        assert_eq!(rpc_err.message, "cannot reset");

        server.join().unwrap();
    }

    #[tokio::test]
    async fn wait_vm_cancellable_and_dropped_wait_unblock_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("hang.sock");
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&SockAddr::unix(&sock_path).unwrap()).unwrap();
        listener.listen(8).unwrap();

        let (peer_closed_tx, peer_closed_rx) = tokio::sync::oneshot::channel::<()>();
        let (peer_closed_tx2, peer_closed_rx2) = tokio::sync::oneshot::channel::<()>();
        let mut txs = [Some(peer_closed_tx), Some(peer_closed_tx2)];
        let server = thread::spawn(move || {
            for slot in &mut txs {
                let (mut conn, _) = listener.accept().unwrap();
                let (_sid, _mtype, _bytes) = read_frame(&mut conn).unwrap();
                // Never reply; wait until the client shuts down/closes the socket.
                let mut buf = [0u8; 16];
                let _ = conn.read(&mut buf);
                if let Some(tx) = slot.take() {
                    let _ = tx.send(());
                }
            }
        });

        let client = TtrpcClient::new(&sock_path);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let wait_task = tokio::spawn({
            let client = client.clone();
            async move { client.wait_vm_cancellable(cancel_rx).await }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = cancel_tx.send(true);
        let outcome = tokio::time::timeout(Duration::from_secs(2), wait_task)
            .await
            .expect("wait_vm_cancellable did not unblock on cancel")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, WaitVmOutcome::Cancelled);
        tokio::time::timeout(Duration::from_secs(2), peer_closed_rx)
            .await
            .expect("server did not observe socket close after wait_vm_cancellable cancel")
            .unwrap();

        // Dropping an uncancelled wait_vm() future must also close the socket immediately.
        {
            let wait_fut = client.wait_vm();
            tokio::pin!(wait_fut);
            tokio::select! {
                _ = &mut wait_fut => panic!("wait_vm unexpectedly completed without server reply"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        tokio::time::timeout(Duration::from_secs(2), peer_closed_rx2)
            .await
            .expect("server did not observe socket close after wait_vm future drop")
            .unwrap();

        server.join().unwrap();
    }
}
