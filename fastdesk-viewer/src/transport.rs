use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    HvSock { vm_id: [u8; 16], service_id: [u8; 16] },
    Vsock { cid: u32, port: u32 },
}

pub struct TransportStream {
    inner: StreamInner,
}

enum StreamInner {
    Tcp(TcpStream),
    #[cfg(any(windows, target_os = "linux"))]
    Socket(socket2::Socket),
}

impl TransportStream {
    pub fn connect(uri: &str) -> anyhow::Result<Self> {
        let endpoint = parse_endpoint(uri)?;
        match endpoint {
            Endpoint::Tcp(addr_str) => {
                let addr: SocketAddr = addr_str
                    .to_socket_addrs()
                    .with_context(|| format!("resolving TCP address '{addr_str}'"))?
                    .next()
                    .ok_or_else(|| anyhow!("no addresses resolved for '{addr_str}'"))?;
                let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
                    .with_context(|| format!("connecting to {addr}"))?;
                stream.set_nodelay(true)?;
                Ok(Self {
                    inner: StreamInner::Tcp(stream),
                })
            }
            Endpoint::HvSock { vm_id, service_id } => connect_hvsock(vm_id, service_id),
            Endpoint::Vsock { cid, port } => connect_vsock(cid, port),
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let inner = match &self.inner {
            StreamInner::Tcp(s) => StreamInner::Tcp(s.try_clone()?),
            #[cfg(any(windows, target_os = "linux"))]
            StreamInner::Socket(s) => StreamInner::Socket(s.try_clone()?),
        };
        Ok(Self { inner })
    }
}

impl Read for TransportStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            StreamInner::Tcp(s) => s.read(buf),
            #[cfg(any(windows, target_os = "linux"))]
            StreamInner::Socket(s) => s.read(buf),
        }
    }
}

impl Write for TransportStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            StreamInner::Tcp(s) => s.write(buf),
            #[cfg(any(windows, target_os = "linux"))]
            StreamInner::Socket(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            StreamInner::Tcp(s) => s.flush(),
            #[cfg(any(windows, target_os = "linux"))]
            StreamInner::Socket(s) => s.flush(),
        }
    }
}

pub fn parse_endpoint(uri: &str) -> anyhow::Result<Endpoint> {
    if let Some(rest) = uri.strip_prefix("tcp://") {
        if rest.is_empty() {
            bail!("empty tcp:// host:port");
        }
        return Ok(Endpoint::Tcp(rest.to_owned()));
    }
    if let Some(rest) = uri.strip_prefix("hvsock://") {
        let (vm_part, svc_part) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("hvsock URI must be hvsock://<vm_guid>/<port_or_guid>"))?;
        let vm_id = match vm_part.to_ascii_lowercase().as_str() {
            "any" | "zero" => [0u8; 16],
            "parent" | "host" => parse_guid_le("a42e7cda-d03f-480c-9cc2-a4de20abb878")?,
            "loopback" => parse_guid_le("e0e16197-dd56-4a10-9195-5ee7a155a838")?,
            other => parse_guid_le(other)?,
        };
        let service_id = if let Ok(port) = svc_part.parse::<u32>() {
            parse_guid_le(&fastdesk_proto::vsock_service_guid_string(port))?
        } else {
            parse_guid_le(svc_part)?
        };
        return Ok(Endpoint::HvSock { vm_id, service_id });
    }
    if let Some(rest) = uri.strip_prefix("vsock://") {
        let (cid_str, port_str) = rest
            .split_once(':')
            .ok_or_else(|| anyhow!("vsock URI must be vsock://<cid>:<port>"))?;
        let cid = match cid_str.to_ascii_lowercase().as_str() {
            "any" => u32::MAX,
            "local" => 1,
            "host" => 2,
            other => other.parse::<u32>().context("invalid vsock CID")?,
        };
        let port = port_str.parse::<u32>().context("invalid vsock port")?;
        return Ok(Endpoint::Vsock { cid, port });
    }
    if uri.contains("://") {
        bail!("unsupported transport scheme in '{uri}'");
    }
    Ok(Endpoint::Tcp(uri.to_owned()))
}

pub fn parse_guid_le(s: &str) -> anyhow::Result<[u8; 16]> {
    let clean = s.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = clean.split('-').collect();
    if parts.len() != 5
        || parts[0].len() != 8
        || parts[1].len() != 4
        || parts[2].len() != 4
        || parts[3].len() != 4
        || parts[4].len() != 12
    {
        bail!("invalid GUID '{s}'");
    }
    let d1 = u32::from_str_radix(parts[0], 16)?;
    let d2 = u16::from_str_radix(parts[1], 16)?;
    let d3 = u16::from_str_radix(parts[2], 16)?;
    let d4_hi = u16::from_str_radix(parts[3], 16)?;
    let d4_lo = u64::from_str_radix(parts[4], 16)?;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&d1.to_le_bytes());
    out[4..6].copy_from_slice(&d2.to_le_bytes());
    out[6..8].copy_from_slice(&d3.to_le_bytes());
    out[8..10].copy_from_slice(&d4_hi.to_be_bytes());
    out[10..16].copy_from_slice(&d4_lo.to_be_bytes()[2..8]);
    Ok(out)
}

#[cfg(windows)]
fn connect_hvsock(vm_id: [u8; 16], service_id: [u8; 16]) -> anyhow::Result<TransportStream> {
    use socket2::SockAddr;
    use socket2::Socket;
    use socket2::Type;
    use windows_sys::Win32::Networking::WinSock::AF_HYPERV;
    use windows_sys::Win32::System::Hypervisor::HV_PROTOCOL_RAW;
    use windows_sys::Win32::System::Hypervisor::SOCKADDR_HV;
    use windows_sys::core::GUID;

    fn to_guid(b: [u8; 16]) -> GUID {
        GUID {
            data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_le_bytes([b[4], b[5]]),
            data3: u16::from_le_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }

    let sock = Socket::new(
        (AF_HYPERV as i32).into(),
        Type::STREAM,
        Some((HV_PROTOCOL_RAW as i32).into()),
    )
    .context("creating AF_HYPERV socket")?;

    let hv_addr = SOCKADDR_HV {
        Family: AF_HYPERV,
        Reserved: 0,
        VmId: to_guid(vm_id),
        ServiceId: to_guid(service_id),
    };

    // SAFETY: SockAddr::try_init initializes the caller-owned storage buffer up to size_of::<SOCKADDR_HV>().
    let (_, sockaddr) = unsafe {
        SockAddr::try_init(|storage, len| {
            let needed = size_of::<SOCKADDR_HV>();
            if (*len as usize) < needed {
                return Err(io::Error::from_raw_os_error(libc_einval()));
            }
            std::ptr::write_unaligned(storage.cast::<SOCKADDR_HV>(), hv_addr);
            *len = needed as i32;
            Ok(())
        })
    }
    .context("initializing SOCKADDR_HV")?;

    sock.connect(&sockaddr).context("connecting AF_HYPERV socket")?;
    Ok(TransportStream {
        inner: StreamInner::Socket(sock),
    })
}

#[cfg(windows)]
fn libc_einval() -> i32 {
    10022 // WSAEINVAL
}

#[cfg(not(windows))]
fn connect_hvsock(_vm_id: [u8; 16], _service_id: [u8; 16]) -> anyhow::Result<TransportStream> {
    bail!("hvsock:// is only supported on Windows hosts/guests")
}

#[cfg(target_os = "linux")]
fn connect_vsock(cid: u32, port: u32) -> anyhow::Result<TransportStream> {
    use socket2::SockAddr;
    use socket2::Socket;
    use socket2::Type;

    let sock = Socket::new(libc::AF_VSOCK.into(), Type::STREAM, None)
        .context("creating AF_VSOCK socket")?;
    let mut vm_addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    vm_addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    vm_addr.svm_port = port;
    vm_addr.svm_cid = cid;

    // SAFETY: SockAddr::try_init initializes storage with libc::sockaddr_vm.
    let (_, sockaddr) = unsafe {
        SockAddr::try_init(|storage, len| {
            let needed = size_of::<libc::sockaddr_vm>();
            if (*len as usize) < needed {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            std::ptr::write_unaligned(storage.cast::<libc::sockaddr_vm>(), vm_addr);
            *len = needed as libc::socklen_t;
            Ok(())
        })
    }
    .context("initializing sockaddr_vm")?;

    sock.connect(&sockaddr).context("connecting AF_VSOCK socket")?;
    Ok(TransportStream {
        inner: StreamInner::Socket(sock),
    })
}

#[cfg(not(target_os = "linux"))]
fn connect_vsock(_cid: u32, _port: u32) -> anyhow::Result<TransportStream> {
    bail!("vsock:// is only supported on Linux; use hvsock:// on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_endpoint_schemes() {
        assert_eq!(
            parse_endpoint("tcp://127.0.0.1:5901").unwrap(),
            Endpoint::Tcp("127.0.0.1:5901".into())
        );
        assert_eq!(
            parse_endpoint("127.0.0.1:5901").unwrap(),
            Endpoint::Tcp("127.0.0.1:5901".into())
        );
        assert_eq!(
            parse_endpoint("vsock://2:5901").unwrap(),
            Endpoint::Vsock { cid: 2, port: 5901 }
        );
        assert_eq!(
            parse_endpoint("vsock://local:5901").unwrap(),
            Endpoint::Vsock { cid: 1, port: 5901 }
        );
        let hv = parse_endpoint("hvsock://parent/5901").unwrap();
        match hv {
            Endpoint::HvSock { vm_id, service_id } => {
                assert_eq!(vm_id, parse_guid_le("a42e7cda-d03f-480c-9cc2-a4de20abb878").unwrap());
                assert_eq!(
                    service_id,
                    parse_guid_le("0000170d-facb-11e6-bd58-64006a7986d3").unwrap()
                );
            }
            other => panic!("unexpected endpoint: {other:?}"),
        }
    }
}
