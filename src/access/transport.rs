use axum::serve::Listener;
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

/// Limits accepted sockets including clients that never finish sending headers.
pub struct BoundedListener {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    lifetime: Duration,
}
impl BoundedListener {
    pub fn new(listener: TcpListener, lifetime: Duration) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(32)),
            lifetime,
        }
    }
}
impl Listener for BoundedListener {
    type Io = BoundedStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = self.permits.clone().acquire_owned().await.unwrap();
        let (stream, address) = <TcpListener as Listener>::accept(&mut self.listener).await;
        (
            BoundedStream {
                stream,
                deadline: Box::pin(tokio::time::sleep(self.lifetime)),
                _permit: permit,
            },
            address,
        )
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}
pub struct BoundedStream {
    stream: TcpStream,
    deadline: Pin<Box<Sleep>>,
    _permit: OwnedSemaphorePermit,
}
impl BoundedStream {
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.deadline.as_mut().poll(cx).is_ready()
    }
}
impl AsyncRead for BoundedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.expired(cx) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}
impl AsyncWrite for BoundedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.expired(cx) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.expired(cx) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
