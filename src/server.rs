//! Binding a local port and serving an axum router on a background runtime, until dropped.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::thread::JoinHandle;

use tokio::sync::oneshot;

#[cfg(feature = "tls")]
use tokio_rustls::rustls::ServerConfig;

/// Worker threads of the runtime that serves the fake.
const WORKERS: usize = 2;

/// The TLS material a server is started with; without the `tls` feature there is none.
#[cfg(feature = "tls")]
type Config = std::sync::Arc<ServerConfig>;
#[cfg(not(feature = "tls"))]
type Config = ();

/// A served router, shut down when dropped.
pub struct Server {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds `address`, for [`Server::serve`], which takes over the listener.
    pub fn bind(address: SocketAddr) -> io::Result<TcpListener> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    /// Serves `router` on `listener` over HTTP, in a background thread of its own.
    pub fn serve(listener: TcpListener, router: axum::Router) -> io::Result<Self> {
        Self::spawn(listener, router, None)
    }

    /// Serves `router` on `listener` over HTTPS with `config`, as [`crate::tls::generate`]
    /// makes.
    #[cfg(feature = "tls")]
    pub fn serve_tls(
        listener: TcpListener,
        router: axum::Router,
        config: Config,
    ) -> io::Result<Self> {
        Self::spawn(listener, router, Some(config))
    }

    fn spawn(
        listener: TcpListener,
        router: axum::Router,
        config: Option<Config>,
    ) -> io::Result<Self> {
        let scheme = if config.is_some() { "https" } else { "http" };
        let url = format!("{scheme}://{}", listener.local_addr()?);
        #[cfg(not(feature = "tls"))]
        let _ = config;
        let (shutdown, stopped) = oneshot::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .enable_all()
            .build()?;
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("cannot adopt listener");
                #[cfg(feature = "tls")]
                if let Some(config) = config {
                    return serve_tls(listener, router, config, stopped).await;
                }
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("server failed");
            });
        });
        Ok(Self {
            url,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    /// Base URL of the server, such as `http://127.0.0.1:1234`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Blocks until the serving thread stops.
    pub fn wait(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Stops serving and waits for the thread, which dropping does anyway.
    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accepts TLS connections and serves each with HTTP/1.1 until `stopped` fires.
#[cfg(feature = "tls")]
async fn serve_tls(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    config: Config,
    stopped: oneshot::Receiver<()>,
) {
    use tower_service::Service as _;
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    tokio::pin!(stopped);
    loop {
        let (stream, address) = tokio::select! {
            _ = &mut stopped => return,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(_) => continue,
            },
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else {
                return;
            };
            let service =
                hyper::service::service_fn(move |request: http::Request<hyper::body::Incoming>| {
                    let mut router = router.clone();
                    let mut request = request.map(axum::body::Body::new);
                    request
                        .extensions_mut()
                        .insert(axum::extract::ConnectInfo(address));
                    async move { router.call(request).await }
                });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}
