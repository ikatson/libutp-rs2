use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    task::{Context, Poll},
};

use tokio::net::UdpSocket;

pub trait Transport: Send + Sync + Unpin + 'static {
    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = std::io::Result<(usize, SocketAddr)>> + Send + Sync + 'a;

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<(usize, SocketAddr)>>;

    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = std::io::Result<usize>> + Send + Sync + 'a;

    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<std::io::Result<usize>>;

    fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize>;

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    // The local address transport is bound to. Used only for logging.
    fn bind_addr(&self) -> SocketAddr;
}

impl Transport for UdpSocket {
    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = std::io::Result<(usize, SocketAddr)>> + Send + Sync + 'a {
        UdpSocket::recv_from(self, buf)
    }

    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = std::io::Result<usize>> + Send + Sync + 'a {
        UdpSocket::send_to(self, buf, target)
    }

    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<std::io::Result<usize>> {
        UdpSocket::poll_send_to(self, cx, buf, target)
    }

    fn bind_addr(&self) -> SocketAddr {
        UdpSocket::local_addr(self).unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
    }

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        UdpSocket::poll_send_ready(self, cx)
    }

    fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        UdpSocket::try_send_to(self, buf, target)
    }

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<(usize, SocketAddr)>> {
        let mut b = tokio::io::ReadBuf::new(buf);
        match UdpSocket::poll_recv_from(&self, cx, &mut b) {
            Poll::Ready(Ok(addr)) => Poll::Ready(Ok((b.filled().len(), addr))),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}
