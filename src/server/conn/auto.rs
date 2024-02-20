//! Http1 or Http2 connection.

use futures_util::ready;
use hyper::service::HttpService;
use std::future::Future;
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{error::Error as StdError, io, time::Duration};

use bytes::Bytes;
use http::{Request, Response};
use http_body::Body;
use hyper::{
    body::Incoming,
    rt::{Read, ReadBuf, Timer, Write},
    service::Service,
};

#[cfg(feature = "http1")]
use hyper::server::conn::http1;

#[cfg(feature = "http2")]
use hyper::{rt::bounds::Http2ServerConnExec, server::conn::http2};

#[cfg(any(not(feature = "http2"), not(feature = "http1")))]
use std::marker::PhantomData;

use pin_project_lite::pin_project;

use crate::common::rewind::Rewind;

type Error = Box<dyn std::error::Error + Send + Sync>;

type Result<T> = std::result::Result<T, Error>;

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Exactly equivalent to [`Http2ServerConnExec`].
#[cfg(feature = "http2")]
pub trait HttpServerConnExec<A, B: Body>: Http2ServerConnExec<A, B> {}

#[cfg(feature = "http2")]
impl<A, B: Body, T: Http2ServerConnExec<A, B>> HttpServerConnExec<A, B> for T {}

/// Exactly equivalent to [`Http2ServerConnExec`].
#[cfg(not(feature = "http2"))]
pub trait HttpServerConnExec<A, B: Body> {}

#[cfg(not(feature = "http2"))]
impl<A, B: Body, T> HttpServerConnExec<A, B> for T {}

/// Http1 or Http2 connection builder.
#[derive(Clone, Debug)]
pub struct Builder<E> {
    #[cfg(feature = "http1")]
    http1: http1::Builder,
    #[cfg(feature = "http2")]
    http2: http2::Builder<E>,
    #[cfg(not(feature = "http2"))]
    _executor: E,
}

impl<E> Builder<E> {
    /// Create a new auto connection builder.
    ///
    /// `executor` parameter should be a type that implements
    /// [`Executor`](hyper::rt::Executor) trait.
    ///
    /// # Example
    ///
    /// ```
    /// use hyper_util::{
    ///     rt::TokioExecutor,
    ///     server::conn::auto,
    /// };
    ///
    /// auto::Builder::new(TokioExecutor::new());
    /// ```
    pub fn new(executor: E) -> Self {
        Self {
            #[cfg(feature = "http1")]
            http1: http1::Builder::new(),
            #[cfg(feature = "http2")]
            http2: http2::Builder::new(executor),
            #[cfg(not(feature = "http2"))]
            _executor: executor,
        }
    }

    /// Http1 configuration.
    #[cfg(feature = "http1")]
    pub fn http1(&mut self) -> Http1Builder<'_, E> {
        Http1Builder { inner: self }
    }

    /// Http2 configuration.
    #[cfg(feature = "http2")]
    pub fn http2(&mut self) -> Http2Builder<'_, E> {
        Http2Builder { inner: self }
    }

    /// Bind a connection together with a [`Service`].
    pub fn serve_connection<I, S, B>(&self, io: I, service: S) -> Connection<'_, I, S, E>
    where
        S: Service<Request<Incoming>, Response = Response<B>>,
        S::Future: 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin + 'static,
        E: HttpServerConnExec<S::Future, B>,
    {
        Connection {
            state: ConnState::ReadVersion {
                read_version: read_version(io),
                builder: self,
                service: Some(service),
            },
        }
    }

    /// Bind a connection together with a [`Service`], with the ability to
    /// handle HTTP upgrades. This requires that the IO object implements
    /// `Send`.
    pub fn serve_connection_with_upgrades<I, S, B>(
        &self,
        io: I,
        service: S,
    ) -> UpgradeableConnection<'_, I, S, E>
    where
        S: Service<Request<Incoming>, Response = Response<B>>,
        S::Future: 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin + Send + 'static,
        E: HttpServerConnExec<S::Future, B>,
    {
        UpgradeableConnection {
            state: UpgradeableConnState::ReadVersion {
                read_version: read_version(io),
                builder: self,
                service: Some(service),
            },
        }
    }
}

#[derive(Copy, Clone)]
enum Version {
    H1,
    H2,
}

impl Version {
    #[must_use]
    #[cfg(any(not(feature = "http2"), not(feature = "http1")))]
    pub fn unsupported(self) -> Error {
        match self {
            Version::H1 => Error::from("HTTP/1 is not supported"),
            Version::H2 => Error::from("HTTP/2 is not supported"),
        }
    }
}

fn read_version<I>(io: I) -> ReadVersion<I>
where
    I: Read + Unpin,
{
    ReadVersion {
        io: Some(io),
        buf: [MaybeUninit::uninit(); 24],
        filled: 0,
        version: Version::H2,
        _pin: PhantomPinned,
    }
}

pin_project! {
    struct ReadVersion<I> {
        io: Option<I>,
        buf: [MaybeUninit<u8>; 24],
        // the amount of `buf` thats been filled
        filled: usize,
        version: Version,
        // Make this future `!Unpin` for compatibility with async trait methods.
        #[pin]
        _pin: PhantomPinned,
    }
}

impl<I> Future for ReadVersion<I>
where
    I: Read + Unpin,
{
    type Output = io::Result<(Version, Rewind<I>)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let mut buf = ReadBuf::uninit(&mut *this.buf);
        // SAFETY: `this.filled` tracks how many bytes have been read (and thus initialized) and
        // we're only advancing by that many.
        unsafe {
            buf.unfilled().advance(*this.filled);
        };

        // We start as H2 and switch to H1 as soon as we don't have the preface.
        while buf.filled().len() < H2_PREFACE.len() {
            let len = buf.filled().len();
            ready!(Pin::new(this.io.as_mut().unwrap()).poll_read(cx, buf.unfilled()))?;
            *this.filled = buf.filled().len();

            // We starts as H2 and switch to H1 when we don't get the preface.
            if buf.filled().len() == len
                || buf.filled()[len..] != H2_PREFACE[len..buf.filled().len()]
            {
                *this.version = Version::H1;
                break;
            }
        }

        let io = this.io.take().unwrap();
        let buf = buf.filled().to_vec();
        Poll::Ready(Ok((
            *this.version,
            Rewind::new_buffered(io, Bytes::from(buf)),
        )))
    }
}

pin_project! {
    /// Connection future.
    pub struct Connection<'a, I, S, E>
    where
        S: HttpService<Incoming>,
    {
        #[pin]
        state: ConnState<'a, I, S, E>,
    }
}

#[cfg(feature = "http1")]
type Http1Connection<I, S> = hyper::server::conn::http1::Connection<Rewind<I>, S>;

#[cfg(not(feature = "http1"))]
type Http1Connection<I, S> = (PhantomData<I>, PhantomData<S>);

#[cfg(feature = "http2")]
type Http2Connection<I, S, E> = hyper::server::conn::http2::Connection<Rewind<I>, S, E>;

#[cfg(not(feature = "http2"))]
type Http2Connection<I, S, E> = (PhantomData<I>, PhantomData<S>, PhantomData<E>);

pin_project! {
    #[project = ConnStateProj]
    enum ConnState<'a, I, S, E>
    where
        S: HttpService<Incoming>,
    {
        ReadVersion {
            #[pin]
            read_version: ReadVersion<I>,
            builder: &'a Builder<E>,
            service: Option<S>,
        },
        H1 {
            #[pin]
            conn: Http1Connection<I, S>,
        },
        H2 {
            #[pin]
            conn: Http2Connection<I, S, E>,
        },
    }
}

impl<I, S, E, B> Connection<'_, I, S, E>
where
    S: HttpService<Incoming, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    E: HttpServerConnExec<S::Future, B>,
{
    /// Start a graceful shutdown process for this connection.
    ///
    /// This `Connection` should continue to be polled until shutdown can finish.
    ///
    /// # Note
    ///
    /// This should only be called while the `Connection` future is still pending. If called after
    /// `Connection::poll` has resolved, this does nothing.
    pub fn graceful_shutdown(self: Pin<&mut Self>) {
        match self.project().state.project() {
            ConnStateProj::ReadVersion { .. } => {}
            #[cfg(feature = "http1")]
            ConnStateProj::H1 { conn } => conn.graceful_shutdown(),
            #[cfg(feature = "http2")]
            ConnStateProj::H2 { conn } => conn.graceful_shutdown(),
            #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
            _ => unreachable!(),
        }
    }
}

impl<I, S, E, B> Future for Connection<'_, I, S, E>
where
    S: Service<Request<Incoming>, Response = Response<B>>,
    S::Future: 'static,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin + 'static,
    E: HttpServerConnExec<S::Future, B>,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut this = self.as_mut().project();

            match this.state.as_mut().project() {
                ConnStateProj::ReadVersion {
                    read_version,
                    builder,
                    service,
                } => {
                    let (version, io) = ready!(read_version.poll(cx))?;
                    let service = service.take().unwrap();
                    match version {
                        #[cfg(feature = "http1")]
                        Version::H1 => {
                            let conn = builder.http1.serve_connection(io, service);
                            this.state.set(ConnState::H1 { conn });
                        }
                        #[cfg(feature = "http2")]
                        Version::H2 => {
                            let conn = builder.http2.serve_connection(io, service);
                            this.state.set(ConnState::H2 { conn });
                        }
                        #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
                        _ => return Poll::Ready(Err(version.unsupported())),
                    }
                }
                #[cfg(feature = "http1")]
                ConnStateProj::H1 { conn } => {
                    return conn.poll(cx).map_err(Into::into);
                }
                #[cfg(feature = "http2")]
                ConnStateProj::H2 { conn } => {
                    return conn.poll(cx).map_err(Into::into);
                }
                #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
                _ => unreachable!(),
            }
        }
    }
}

pin_project! {
    /// Connection future.
    pub struct UpgradeableConnection<'a, I, S, E>
    where
        S: HttpService<Incoming>,
    {
        #[pin]
        state: UpgradeableConnState<'a, I, S, E>,
    }
}

#[cfg(feature = "http1")]
type Http1UpgradeableConnection<I, S> = hyper::server::conn::http1::UpgradeableConnection<I, S>;

#[cfg(not(feature = "http1"))]
type Http1UpgradeableConnection<I, S> = (PhantomData<I>, PhantomData<S>);

pin_project! {
    #[project = UpgradeableConnStateProj]
    enum UpgradeableConnState<'a, I, S, E>
    where
        S: HttpService<Incoming>,
    {
        ReadVersion {
            #[pin]
            read_version: ReadVersion<I>,
            builder: &'a Builder<E>,
            service: Option<S>,
        },
        H1 {
            #[pin]
            conn: Http1UpgradeableConnection<Rewind<I>, S>,
        },
        H2 {
            #[pin]
            conn: Http2Connection<I, S, E>,
        },
    }
}

impl<I, S, E, B> UpgradeableConnection<'_, I, S, E>
where
    S: HttpService<Incoming, ResBody = B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    E: HttpServerConnExec<S::Future, B>,
{
    /// Start a graceful shutdown process for this connection.
    ///
    /// This `UpgradeableConnection` should continue to be polled until shutdown can finish.
    ///
    /// # Note
    ///
    /// This should only be called while the `Connection` future is still nothing. pending. If
    /// called after `UpgradeableConnection::poll` has resolved, this does nothing.
    pub fn graceful_shutdown(self: Pin<&mut Self>) {
        match self.project().state.project() {
            UpgradeableConnStateProj::ReadVersion { .. } => {}
            #[cfg(feature = "http1")]
            UpgradeableConnStateProj::H1 { conn } => conn.graceful_shutdown(),
            #[cfg(feature = "http2")]
            UpgradeableConnStateProj::H2 { conn } => conn.graceful_shutdown(),
            #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
            _ => unreachable!(),
        }
    }
}

impl<I, S, E, B> Future for UpgradeableConnection<'_, I, S, E>
where
    S: Service<Request<Incoming>, Response = Response<B>>,
    S::Future: 'static,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: Read + Write + Unpin + Send + 'static,
    E: HttpServerConnExec<S::Future, B>,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut this = self.as_mut().project();

            match this.state.as_mut().project() {
                UpgradeableConnStateProj::ReadVersion {
                    read_version,
                    builder,
                    service,
                } => {
                    let (version, io) = ready!(read_version.poll(cx))?;
                    let service = service.take().unwrap();
                    match version {
                        #[cfg(feature = "http1")]
                        Version::H1 => {
                            let conn = builder.http1.serve_connection(io, service).with_upgrades();
                            this.state.set(UpgradeableConnState::H1 { conn });
                        }
                        #[cfg(feature = "http2")]
                        Version::H2 => {
                            let conn = builder.http2.serve_connection(io, service);
                            this.state.set(UpgradeableConnState::H2 { conn });
                        }
                        #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
                        _ => return Poll::Ready(Err(version.unsupported())),
                    }
                }
                #[cfg(feature = "http1")]
                UpgradeableConnStateProj::H1 { conn } => {
                    return conn.poll(cx).map_err(Into::into);
                }
                #[cfg(feature = "http2")]
                UpgradeableConnStateProj::H2 { conn } => {
                    return conn.poll(cx).map_err(Into::into);
                }
                #[cfg(any(not(feature = "http1"), not(feature = "http2")))]
                _ => unreachable!(),
            }
        }
    }
}

/// Http1 part of builder.
#[cfg(feature = "http1")]
pub struct Http1Builder<'a, E> {
    inner: &'a mut Builder<E>,
}

#[cfg(feature = "http1")]
impl<E> Http1Builder<'_, E> {
    /// Http2 configuration.
    #[cfg(feature = "http2")]
    pub fn http2(&mut self) -> Http2Builder<'_, E> {
        Http2Builder { inner: self.inner }
    }

    /// Set whether HTTP/1 connections should support half-closures.
    ///
    /// Clients can chose to shutdown their write-side while waiting
    /// for the server to respond. Setting this to `true` will
    /// prevent closing the connection immediately if `read`
    /// detects an EOF in the middle of a request.
    ///
    /// Default is `false`.
    pub fn half_close(&mut self, val: bool) -> &mut Self {
        self.inner.http1.half_close(val);
        self
    }

    /// Enables or disables HTTP/1 keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.inner.http1.keep_alive(val);
        self
    }

    /// Set whether HTTP/1 connections will write header names as title case at
    /// the socket level.
    ///
    /// Note that this setting does not affect HTTP/2.
    ///
    /// Default is false.
    pub fn title_case_headers(&mut self, enabled: bool) -> &mut Self {
        self.inner.http1.title_case_headers(enabled);
        self
    }

    /// Set whether to support preserving original header cases.
    ///
    /// Currently, this will record the original cases received, and store them
    /// in a private extension on the `Request`. It will also look for and use
    /// such an extension in any provided `Response`.
    ///
    /// Since the relevant extension is still private, there is no way to
    /// interact with the original cases. The only effect this can have now is
    /// to forward the cases in a proxy-like fashion.
    ///
    /// Note that this setting does not affect HTTP/2.
    ///
    /// Default is false.
    pub fn preserve_header_case(&mut self, enabled: bool) -> &mut Self {
        self.inner.http1.preserve_header_case(enabled);
        self
    }

    /// Set a timeout for reading client request headers. If a client does not
    /// transmit the entire header within this time, the connection is closed.
    ///
    /// Default is None.
    pub fn header_read_timeout(&mut self, read_timeout: Duration) -> &mut Self {
        self.inner.http1.header_read_timeout(read_timeout);
        self
    }

    /// Set whether HTTP/1 connections should try to use vectored writes,
    /// or always flatten into a single buffer.
    ///
    /// Note that setting this to false may mean more copies of body data,
    /// but may also improve performance when an IO transport doesn't
    /// support vectored writes well, such as most TLS implementations.
    ///
    /// Setting this to true will force hyper to use queued strategy
    /// which may eliminate unnecessary cloning on some TLS backends
    ///
    /// Default is `auto`. In this mode hyper will try to guess which
    /// mode to use
    pub fn writev(&mut self, val: bool) -> &mut Self {
        self.inner.http1.writev(val);
        self
    }

    /// Set the maximum buffer size for the connection.
    ///
    /// Default is ~400kb.
    ///
    /// # Panics
    ///
    /// The minimum value allowed is 8192. This method panics if the passed `max` is less than the minimum.
    pub fn max_buf_size(&mut self, max: usize) -> &mut Self {
        self.inner.http1.max_buf_size(max);
        self
    }

    /// Aggregates flushes to better support pipelined responses.
    ///
    /// Experimental, may have bugs.
    ///
    /// Default is false.
    pub fn pipeline_flush(&mut self, enabled: bool) -> &mut Self {
        self.inner.http1.pipeline_flush(enabled);
        self
    }

    /// Set the timer used in background tasks.
    pub fn timer<M>(&mut self, timer: M) -> &mut Self
    where
        M: Timer + Send + Sync + 'static,
    {
        self.inner.http1.timer(timer);
        self
    }

    /// Bind a connection together with a [`Service`].
    #[cfg(feature = "http2")]
    pub async fn serve_connection<I, S, B>(&self, io: I, service: S) -> Result<()>
    where
        S: Service<Request<Incoming>, Response = Response<B>>,
        S::Future: 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin + 'static,
        E: HttpServerConnExec<S::Future, B>,
    {
        self.inner.serve_connection(io, service).await
    }

    /// Bind a connection together with a [`Service`].
    #[cfg(not(feature = "http2"))]
    pub async fn serve_connection<I, S, B>(&self, io: I, service: S) -> Result<()>
    where
        S: Service<Request<Incoming>, Response = Response<B>>,
        S::Future: 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin + 'static,
    {
        self.inner.serve_connection(io, service).await
    }
}

/// Http2 part of builder.
#[cfg(feature = "http2")]
pub struct Http2Builder<'a, E> {
    inner: &'a mut Builder<E>,
}

#[cfg(feature = "http2")]
impl<E> Http2Builder<'_, E> {
    #[cfg(feature = "http1")]
    /// Http1 configuration.
    pub fn http1(&mut self) -> Http1Builder<'_, E> {
        Http1Builder { inner: self.inner }
    }

    /// Sets the [`SETTINGS_INITIAL_WINDOW_SIZE`][spec] option for HTTP2
    /// stream-level flow control.
    ///
    /// Passing `None` will do nothing.
    ///
    /// If not set, hyper will use a default.
    ///
    /// [spec]: https://http2.github.io/http2-spec/#SETTINGS_INITIAL_WINDOW_SIZE
    pub fn initial_stream_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        self.inner.http2.initial_stream_window_size(sz);
        self
    }

    /// Sets the max connection-level flow control for HTTP2.
    ///
    /// Passing `None` will do nothing.
    ///
    /// If not set, hyper will use a default.
    pub fn initial_connection_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        self.inner.http2.initial_connection_window_size(sz);
        self
    }

    /// Sets whether to use an adaptive flow control.
    ///
    /// Enabling this will override the limits set in
    /// `http2_initial_stream_window_size` and
    /// `http2_initial_connection_window_size`.
    pub fn adaptive_window(&mut self, enabled: bool) -> &mut Self {
        self.inner.http2.adaptive_window(enabled);
        self
    }

    /// Sets the maximum frame size to use for HTTP2.
    ///
    /// Passing `None` will do nothing.
    ///
    /// If not set, hyper will use a default.
    pub fn max_frame_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        self.inner.http2.max_frame_size(sz);
        self
    }

    /// Sets the [`SETTINGS_MAX_CONCURRENT_STREAMS`][spec] option for HTTP2
    /// connections.
    ///
    /// Default is 200. Passing `None` will remove any limit.
    ///
    /// [spec]: https://http2.github.io/http2-spec/#SETTINGS_MAX_CONCURRENT_STREAMS
    pub fn max_concurrent_streams(&mut self, max: impl Into<Option<u32>>) -> &mut Self {
        self.inner.http2.max_concurrent_streams(max);
        self
    }

    /// Sets an interval for HTTP2 Ping frames should be sent to keep a
    /// connection alive.
    ///
    /// Pass `None` to disable HTTP2 keep-alive.
    ///
    /// Default is currently disabled.
    ///
    /// # Cargo Feature
    ///
    pub fn keep_alive_interval(&mut self, interval: impl Into<Option<Duration>>) -> &mut Self {
        self.inner.http2.keep_alive_interval(interval);
        self
    }

    /// Sets a timeout for receiving an acknowledgement of the keep-alive ping.
    ///
    /// If the ping is not acknowledged within the timeout, the connection will
    /// be closed. Does nothing if `http2_keep_alive_interval` is disabled.
    ///
    /// Default is 20 seconds.
    ///
    /// # Cargo Feature
    ///
    pub fn keep_alive_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.inner.http2.keep_alive_timeout(timeout);
        self
    }

    /// Set the maximum write buffer size for each HTTP/2 stream.
    ///
    /// Default is currently ~400KB, but may change.
    ///
    /// # Panics
    ///
    /// The value must be no larger than `u32::MAX`.
    pub fn max_send_buf_size(&mut self, max: usize) -> &mut Self {
        self.inner.http2.max_send_buf_size(max);
        self
    }

    /// Enables the [extended CONNECT protocol].
    ///
    /// [extended CONNECT protocol]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
    pub fn enable_connect_protocol(&mut self) -> &mut Self {
        self.inner.http2.enable_connect_protocol();
        self
    }

    /// Sets the max size of received header frames.
    ///
    /// Default is currently ~16MB, but may change.
    pub fn max_header_list_size(&mut self, max: u32) -> &mut Self {
        self.inner.http2.max_header_list_size(max);
        self
    }

    /// Set the timer used in background tasks.
    pub fn timer<M>(&mut self, timer: M) -> &mut Self
    where
        M: Timer + Send + Sync + 'static,
    {
        self.inner.http2.timer(timer);
        self
    }

    /// Bind a connection together with a [`Service`].
    pub async fn serve_connection<I, S, B>(&self, io: I, service: S) -> Result<()>
    where
        S: Service<Request<Incoming>, Response = Response<B>>,
        S::Future: 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: Read + Write + Unpin + 'static,
        E: HttpServerConnExec<S::Future, B>,
    {
        self.inner.serve_connection(io, service).await
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto,
    };
    use http::{Request, Response};
    use http_body::Body;
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::{body, body::Bytes, client, service::service_fn};
    use std::{convert::Infallible, error::Error as StdError, net::SocketAddr};
    use tokio::net::{TcpListener, TcpStream};

    const BODY: &[u8] = b"Hello, world!";

    #[test]
    fn configuration() {
        // One liner.
        auto::Builder::new(TokioExecutor::new())
            .http1()
            .keep_alive(true)
            .http2()
            .keep_alive_interval(None);
        //  .serve_connection(io, service);

        // Using variable.
        let mut builder = auto::Builder::new(TokioExecutor::new());

        builder.http1().keep_alive(true);
        builder.http2().keep_alive_interval(None);
        // builder.serve_connection(io, service);
    }

    #[cfg(not(miri))]
    #[tokio::test]
    async fn http1() {
        let addr = start_server().await;
        let mut sender = connect_h1(addr).await;

        let response = sender
            .send_request(Request::new(Empty::<Bytes>::new()))
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert_eq!(body, BODY);
    }

    #[cfg(not(miri))]
    #[tokio::test]
    async fn http2() {
        let addr = start_server().await;
        let mut sender = connect_h2(addr).await;

        let response = sender
            .send_request(Request::new(Empty::<Bytes>::new()))
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();

        assert_eq!(body, BODY);
    }

    async fn connect_h1<B>(addr: SocketAddr) -> client::conn::http1::SendRequest<B>
    where
        B: Body + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        let stream = TokioIo::new(TcpStream::connect(addr).await.unwrap());
        let (sender, connection) = client::conn::http1::handshake(stream).await.unwrap();

        tokio::spawn(connection);

        sender
    }

    async fn connect_h2<B>(addr: SocketAddr) -> client::conn::http2::SendRequest<B>
    where
        B: Body + Unpin + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        let stream = TokioIo::new(TcpStream::connect(addr).await.unwrap());
        let (sender, connection) = client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(stream)
            .await
            .unwrap();

        tokio::spawn(connection);

        sender
    }

    async fn start_server() -> SocketAddr {
        let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
        let listener = TcpListener::bind(addr).await.unwrap();

        let local_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let stream = TokioIo::new(stream);
                tokio::task::spawn(async move {
                    let _ = auto::Builder::new(TokioExecutor::new())
                        .serve_connection(stream, service_fn(hello))
                        .await;
                });
            }
        });

        local_addr
    }

    async fn hello(_req: Request<body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        Ok(Response::new(Full::new(Bytes::from(BODY))))
    }
}
