//! A remote TCP stream may be carried by a direct socket or an SSH channel.
use super::ssh::connection::DirectTcpStream;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
pub(crate) enum NetworkStream {
    Tcp(TcpStream),
    Ssh(DirectTcpStream),
}
impl NetworkStream {
    pub fn peer_addr(&self) -> io::Result<Option<SocketAddr>> {
        match self {
            Self::Tcp(stream) => stream.peer_addr().map(Some),
            Self::Ssh(_) => Ok(None),
        }
    }
}
impl AsyncRead for NetworkStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Ssh(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for NetworkStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Ssh(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            Self::Ssh(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Ssh(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}
