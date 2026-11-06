//! Bounded loopback transport admission; permits live exactly as long as accepted IO.
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

pub(super) struct Listener {
    inner: TcpListener,
    permits: Arc<Semaphore>,
}
impl Listener {
    pub(super) fn new(inner: TcpListener) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(32)),
        }
    }
}
impl axum::serve::Listener for Listener {
    type Io = Connection;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // This semaphore is never closed. Admission precedes accept/connection tasks.
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("listener admission remains open");
        let (inner, address) = axum::serve::Listener::accept(&mut self.inner).await;
        (
            Connection {
                inner,
                _permit: permit,
                idle: Box::pin(tokio::time::sleep(Duration::from_secs(30))),
            },
            address,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(super) struct Connection {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
    idle: Pin<Box<Sleep>>,
}
impl Connection {
    fn timeout(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.idle.as_mut().poll(cx).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "inspector connection idle",
            ));
        }
        Ok(())
    }
    fn progress(&mut self) {
        self.idle
            .as_mut()
            .reset(Instant::now() + Duration::from_secs(30));
    }
}
impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.timeout(cx)?;
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.progress();
        }
        result
    }
}
impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.timeout(cx)?;
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(length)) if length > 0) {
            self.progress();
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.timeout(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
