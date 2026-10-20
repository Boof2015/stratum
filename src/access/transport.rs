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
/// The installed TLS server uses the same socket lifetime bound. Admission is
/// immediate so excess accepted sockets do not accumulate while waiting for a slot.
#[derive(Clone)]
pub(crate) struct BoundedAcceptor {
    permits: Arc<Semaphore>,
    lifetime: Duration,
}
impl BoundedAcceptor {
    pub(crate) fn new(limit: usize, lifetime: Duration) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            lifetime,
        }
    }
}
impl<S> axum_server::accept::Accept<TcpStream, S> for BoundedAcceptor {
    type Stream = BoundedStream;
    type Service = S;
    type Future = std::future::Ready<io::Result<(Self::Stream, S)>>;
    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        std::future::ready(
            self.permits
                .clone()
                .try_acquire_owned()
                .map(|permit| {
                    (
                        BoundedStream {
                            stream,
                            deadline: Box::pin(tokio::time::sleep(self.lifetime)),
                            _permit: permit,
                        },
                        service,
                    )
                })
                .map_err(|_| io::ErrorKind::ConnectionRefused.into()),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum_server::accept::Accept;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn installed_acceptor_bounds_sockets_and_releases_expired_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let acceptor = BoundedAcceptor::new(1, Duration::from_millis(100));
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (mut bounded, ()) = acceptor.accept(socket, ()).await.unwrap();
        let _other = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(socket, ()).await.is_err());
        let error = tokio::time::timeout(Duration::from_secs(1), bounded.read_u8())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(bounded);
        let _next = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(socket, ()).await.is_ok());
    }
}
