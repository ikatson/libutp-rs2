use std::{
    net::SocketAddr,
    task::{Context, Poll},
};

use tokio::net::UdpSocket;

pub trait Transport: Send + Sync + Unpin + 'static {
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<(usize, SocketAddr)>>;

    fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize>;

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;
}

impl Transport for UdpSocket {
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
        match UdpSocket::poll_recv_from(self, cx, &mut b) {
            Poll::Ready(Ok(addr)) => Poll::Ready(Ok((b.filled().len(), addr))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}
