#![feature(core_intrinsics, destructuring_assignment)]

// TODO: split into multiple processes, with multiple uids (stretch goal: do not
// require root and allow the user to directly call multiple executables and
// pass it the pre-opened sockets)

// TODO: make everything configurable, and actually implement the wasm scheme
// described in the docs

use std::{io, path::PathBuf, pin::Pin, rc::Rc, sync::Arc, time::Duration};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use chrono::Utc;
use easy_parallel::Parallel;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt};
use scoped_tls::scoped_thread_local;
use smol::unblock;
use structopt::StructOpt;
use tracing::{error, info, warn};

use smtp_message::{Email, Hostname};
use smtp_queue::QueueId;
use smtp_queue_fs::FsStorage;
use smtp_server::{reply, Decision};

const NUM_THREADS: usize = 4;
const QUEUE_DIR: &str = "/tmp/kannader/queue";
const CERT_FILE: &str = "/tmp/kannader/cert.pem";
const KEY_FILE: &str = "/tmp/kannader/key.pem";

const DATABUF_SIZE: usize = 16 * 1024;

#[derive(serde::Deserialize, serde::Serialize)]
struct Meta;

type DynAsyncReadWrite =
    duplexify::Duplex<Pin<Box<dyn Send + AsyncRead>>, Pin<Box<dyn Send + AsyncWrite>>>;

struct NoCertVerifier;

impl rustls::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _roots: &rustls::RootCertStore,
        _presented_certs: &[rustls::Certificate],
        _dns_name: webpki::DNSNameRef,
        _ocsp_response: &[u8],
    ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
        Ok(rustls::ServerCertVerified::assertion())
    }
}

mod server_config {
    kannader_config_types::server_config_implement_host!();
}

struct WasmConfig {
    server_config: server_config::HostSide,
}

impl WasmConfig {
    fn new(engine: &wasmtime::Engine, module: &wasmtime::Module) -> anyhow::Result<WasmConfig> {
        let store = wasmtime::Store::new(engine);
        let instance = wasmtime::Instance::new(&store, module, &[])
            .context("Instantiating the wasm configuration blob")?;

        macro_rules! get_func {
            ($getter:ident, $function:expr) => {
                instance
                    .get_func($function)
                    .ok_or_else(|| anyhow!("Failed to find function export ‘{}’", $function))?
                    .$getter()
                    .with_context(|| format!("Checking the type of ‘{}’", $function))?
            };
        }

        // Parameter: size of the block to allocate
        // Return: address of the allocated block
        let allocate = Rc::new(get_func!(get1, "allocate"));

        // Parameters: (address, size) of the block to deallocate
        let deallocate = Rc::new(get_func!(get2, "deallocate"));

        Ok(WasmConfig {
            server_config: server_config::build_host_side(&instance, allocate, deallocate)
                .context("Getting server configuration")?,
        })
    }
}

scoped_thread_local!(static WASM_CONFIG: WasmConfig);

struct ClientConfig {
    connector: async_tls::TlsConnector,
}

impl ClientConfig {
    fn new(connector: async_tls::TlsConnector) -> ClientConfig {
        ClientConfig { connector }
    }
}

#[async_trait]
impl smtp_client::Config for ClientConfig {
    fn ehlo_hostname(&self) -> Hostname<&str> {
        // TODO: this is ugly
        Hostname::parse(b"localhost")
            .expect("failed parsing static str")
            .1
    }

    async fn tls_connect<IO>(&self, io: IO) -> io::Result<DynAsyncReadWrite>
    where
        IO: 'static + Unpin + Send + AsyncRead + AsyncWrite,
    {
        let io = self.connector.connect("nodomainyet", io).await?;
        let (r, w) = io.split();
        let io = duplexify::Duplex::new(
            Box::pin(r) as Pin<Box<dyn Send + AsyncRead>>,
            Box::pin(w) as Pin<Box<dyn Send + AsyncWrite>>,
        );
        Ok(io)
    }
}

struct QueueConfig;

#[async_trait]
impl smtp_queue::Config<Meta, smtp_queue_fs::Error> for QueueConfig {
    async fn next_interval(&self, _s: smtp_queue::ScheduleInfo) -> Option<Duration> {
        // TODO: most definitely should try again
        // TODO: add bounce support to both transport and here
        None
    }

    async fn log_storage_error(&self, err: smtp_queue_fs::Error, id: Option<QueueId>) {
        error!(queue_id = ?id, error = ?anyhow::Error::new(err), "Storage error");
    }

    async fn log_found_inflight(&self, inflight: QueueId) {
        warn!(queue_id=?inflight, "Found inflight mail, waiting {:?} before sending", self.found_inflight_check_delay());
    }

    async fn log_found_pending_cleanup(&self, pcm: QueueId) {
        warn!(queue_id=?pcm, "Found mail pending cleanup");
    }

    async fn log_queued_mail_vanished(&self, id: QueueId) {
        error!(queue_id = ?id, "Queued mail vanished");
    }

    async fn log_inflight_mail_vanished(&self, id: QueueId) {
        error!(queue_id = ?id, "Inflight mail vanished");
    }

    async fn log_pending_cleanup_mail_vanished(&self, id: QueueId) {
        error!(queue_id = ?id, "Mail that was pending cleanup vanished");
    }

    async fn log_too_big_duration(&self, id: QueueId, too_big: Duration, new: Duration) {
        error!(queue_id = ?id, too_big = ?too_big, reset_to = ?new, "Ended up having too big a duration");
    }
}

fn transport_error_client_to_queue(
    err: smtp_client::TransportError,
    text: &'static str,
) -> smtp_queue::TransportFailure {
    let severity = err.severity();
    warn!(error = ?anyhow::Error::new(err), "{}", text);
    match severity {
        smtp_client::TransportErrorSeverity::Local => smtp_queue::TransportFailure::Local,
        smtp_client::TransportErrorSeverity::NetworkTransient => {
            smtp_queue::TransportFailure::NetworkTransient
        }
        smtp_client::TransportErrorSeverity::MailTransient => {
            smtp_queue::TransportFailure::MailTransient
        }
        smtp_client::TransportErrorSeverity::MailboxTransient => {
            smtp_queue::TransportFailure::MailboxTransient
        }
        smtp_client::TransportErrorSeverity::MailSystemTransient => {
            smtp_queue::TransportFailure::MailSystemTransient
        }
        smtp_client::TransportErrorSeverity::MailPermanent => {
            smtp_queue::TransportFailure::MailPermanent
        }
        smtp_client::TransportErrorSeverity::MailboxPermanent => {
            smtp_queue::TransportFailure::MailboxPermanent
        }
        smtp_client::TransportErrorSeverity::MailSystemPermanent => {
            smtp_queue::TransportFailure::MailSystemPermanent
        }
    }
}

struct QueueTransport<C, P>(smtp_client::Client<C, P, ClientConfig>)
where
    C: trust_dns_resolver::proto::DnsHandle,
    P: trust_dns_resolver::ConnectionProvider<Conn = C>;

#[async_trait]
impl<C, P> smtp_queue::Transport<Meta> for QueueTransport<C, P>
where
    C: trust_dns_resolver::proto::DnsHandle,
    P: trust_dns_resolver::ConnectionProvider<Conn = C>,
{
    type Destination = smtp_client::Destination;
    type Sender = QueueTransportSender;

    async fn destination(
        &self,
        meta: &smtp_queue::MailMetadata<Meta>,
    ) -> Result<Self::Destination, smtp_queue::TransportFailure> {
        // TODO: this should most likely be a const or similar; and definitely not
        // recomputed on each call to destination
        let localhost = Hostname::parse(b"localhost")
            .expect("failed to parse constant hostname")
            .1
            .to_owned();
        self.0
            .get_destination(meta.to.hostname.as_ref().unwrap_or(&localhost))
            .await
            .map_err(|e| {
                transport_error_client_to_queue(
                    e,
                    "Transport error while trying to get destination",
                )
            })
    }

    async fn connect(
        &self,
        dest: &Self::Destination,
    ) -> Result<Self::Sender, smtp_queue::TransportFailure> {
        info!(destination = %dest, "Connecting to remote server");
        // TODO: log the IP to which we're connecting
        self.0
            .connect(dest)
            .await
            .map(QueueTransportSender)
            .map_err(|e| {
                transport_error_client_to_queue(
                    e,
                    "Transport error while trying to connect to destination",
                )
            })
    }
}

struct QueueTransportSender(smtp_client::Sender<ClientConfig>);

#[async_trait]
impl smtp_queue::TransportSender<Meta> for QueueTransportSender {
    async fn send<Reader>(
        &mut self,
        meta: &smtp_queue::MailMetadata<Meta>,
        mail: Reader,
    ) -> Result<(), smtp_queue::TransportFailure>
    where
        Reader: Send + AsyncRead,
    {
        // TODO: pass through mail id so that it's possible to log it
        self.0
            .send(meta.from.as_ref(), &meta.to, mail)
            .await
            .map_err(|e| {
                transport_error_client_to_queue(e, "Transport error while trying to send email")
            })
    }
}

struct ServerConfig<T>
where
    T: smtp_queue::Transport<Meta>,
{
    acceptor: async_tls::TlsAcceptor,
    queue: smtp_queue::Queue<Meta, QueueConfig, FsStorage<Meta>, T>,
}

#[async_trait]
impl<T> smtp_server::Config for ServerConfig<T>
where
    T: smtp_queue::Transport<Meta>,
{
    type ConnectionUserMeta = Vec<u8>;
    type MailUserMeta = Vec<u8>;

    fn hostname(&self, _conn_meta: &smtp_server::ConnectionMetadata<Vec<u8>>) -> &str {
        "localhost"
    }

    // TODO: this could have a default implementation if we were able to have a
    // default type of () for MailUserMeta without requiring unstable
    async fn new_mail(
        &self,
        _conn_meta: &mut smtp_server::ConnectionMetadata<Self::ConnectionUserMeta>,
    ) -> Self::MailUserMeta {
        Vec::new() // TODO
    }

    // TODO: when GATs are here, we can remove the trait object and return
    // Self::TlsStream<IO> (or maybe we should refactor Config to be Config<IO>? but
    // that's ugly). At that time we can probably get rid of all that duplexify
    // mess... or maybe when we can do trait objects with more than one trait
    /// Note: if you don't want to implement TLS, you should override
    /// `can_do_tls` to return `false` so that STARTTLS is not advertized. This
    /// being said, returning an error here should have the same result in
    /// practice, except clients will try STARTTLS and fail
    async fn tls_accept<IO>(
        &self,
        io: IO,
        _conn_meta: &mut smtp_server::ConnectionMetadata<Self::ConnectionUserMeta>,
    ) -> io::Result<
        duplexify::Duplex<Pin<Box<dyn Send + AsyncRead>>, Pin<Box<dyn Send + AsyncWrite>>>,
    >
    where
        IO: 'static + Unpin + Send + AsyncRead + AsyncWrite,
    {
        let io = self.acceptor.accept(io).await?;
        let (r, w) = io.split();
        let io = duplexify::Duplex::new(
            Box::pin(r) as Pin<Box<dyn Send + AsyncRead>>,
            Box::pin(w) as Pin<Box<dyn Send + AsyncWrite>>,
        );
        Ok(io)
    }

    async fn filter_from(
        &self,
        from: Option<Email>,
        meta: &mut smtp_server::MailMetadata<Self::MailUserMeta>,
        conn_meta: &mut smtp_server::ConnectionMetadata<Self::ConnectionUserMeta>,
    ) -> Decision<Option<Email>> {
        // TODO: have this communication schema for all hooks
        WASM_CONFIG.with(|wasm_config| {
            let res = (wasm_config.server_config.filter_from)(from, meta, conn_meta);
            match res {
                Ok(res) => res.into(),
                Err(e) => {
                    error!(error = ?e, "Internal server error in ‘filter_from’");
                    Decision::Reject {
                        reply: reply::internal_server_error().convert(),
                    }
                }
            }
        })
    }

    async fn filter_to(
        &self,
        to: Email,
        _meta: &mut smtp_server::MailMetadata<Self::MailUserMeta>,
        _conn_meta: &mut smtp_server::ConnectionMetadata<Self::ConnectionUserMeta>,
    ) -> Decision<Email> {
        // TODO: this is BAD
        Decision::Accept {
            reply: reply::okay_to().convert(),
            res: to,
        }
    }

    /// Note: the EscapedDataReader has an inner buffer size of
    /// [`RDBUF_SIZE`](RDBUF_SIZE), which means that reads should not happen
    /// with more than this buffer size.
    ///
    /// Also, note that there is no timeout applied here, so the implementation
    /// of this function is responsible for making sure that the client does not
    /// just stop sending anything to DOS the system.
    async fn handle_mail<'a, R>(
        &self,
        stream: &mut smtp_message::EscapedDataReader<'a, R>,
        meta: smtp_server::MailMetadata<Self::MailUserMeta>,
        _conn_meta: &mut smtp_server::ConnectionMetadata<Self::ConnectionUserMeta>,
    ) -> Decision<()>
    where
        R: Send + Unpin + AsyncRead,
    {
        let mut enqueuer = match self.queue.enqueue().await {
            Ok(enqueuer) => enqueuer,
            Err(e) => {
                error!(error = ?anyhow::Error::new(e), "Internal server error while opening an enqueuer");
                return Decision::Reject {
                    reply: reply::internal_server_error().convert(),
                };
            }
        };
        // TODO: MUST add Received header at least
        // TODO: factor out with the similar logic in smtp-client
        let mut buf = [0; DATABUF_SIZE];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => {
                    // End of stream
                    break;
                }
                Ok(n) => {
                    // Got n bytes
                    if let Err(e) = enqueuer.write_all(&buf[..n]).await {
                        error!(error = ?e, "Internal server error while writing data to queue");
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(_) => (),
                                Err(e) => {
                                    error!(error = ?e, "Internal server error while reading data from network");
                                    break;
                                }
                            }
                        }
                        return Decision::Reject {
                            reply: reply::internal_server_error().convert(),
                        };
                    }
                }
                Err(e) => {
                    error!(error = ?e, "Internal server error while reading data from network");
                    return Decision::Reject {
                        reply: reply::internal_server_error().convert(),
                    };
                }
            }
        }

        if !stream.is_finished() {
            // Stream isn't finished, as we read until end-of-stream it means that there was
            // an error somewhere
            error!("Stream stopped returning any bytes without actually finishing");
            Decision::Reject {
                reply: reply::internal_server_error().convert(),
            }
        } else {
            // Stream is finished, let's complete it then commit the file to the queue and
            // acept
            stream.complete();
            let from = &meta.from;
            let destinations = meta
                .to
                .into_iter()
                .map(move |to| {
                    (
                        smtp_queue::MailMetadata {
                            from: from.clone(),
                            to,
                            metadata: Meta,
                        },
                        smtp_queue::ScheduleInfo {
                            at: Utc::now(),
                            last_attempt: None,
                        },
                    )
                })
                .collect();
            if let Err(e) = enqueuer.commit(destinations).await {
                error!(error = ?e, "Internal server error while committing mail");
                Decision::Reject {
                    reply: reply::internal_server_error().convert(),
                }
            } else {
                Decision::Accept {
                    reply: reply::okay_mail().convert(),
                    res: (),
                }
            }
        }
    }
}

#[derive(structopt::StructOpt)]
#[structopt(
    name = "kannader",
    about = "A highly configurable SMTP server written in Rust."
)]
struct Opt {
    /// Path to the wasm configuration blob
    #[structopt(
        short,
        long,
        parse(from_os_str),
        default_value = "/etc/kannader/config.wasm"
    )]
    // TODO: have wasm configuration blobs pre-provided in /usr/lib or similar
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    // Parse configuration
    let opt = Opt::from_args();

    // Setup logging
    tracing_subscriber::fmt::init();
    info!("Kannader starting up");

    // Load the configuration and run WasmConfig::new once to make sure errors are
    // caught early on
    // TODO: limit the stack size, and make sure we always build with all
    // optimizations
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::from_file(&engine, &opt.config)
        .context("Compiling the wasm configuration blob")?;
    WasmConfig::new(&engine, &module).context("Linking the wasm configuration blob")?;

    // Start the executor
    let ex = Arc::new(smol::Executor::new());

    // TODO: figure out a better shutdown story than brutally killing the server
    // (ie. trigger signal not only when the socket fails)
    let (signal, shutdown) = smol::channel::unbounded::<()>();

    let (_, res): (_, anyhow::Result<()>) = Parallel::new()
        .each(0..NUM_THREADS, |_| {
            let wasm_config =
                WasmConfig::new(&engine, &module).context("Linking the wasm configuration blob")?;
            WASM_CONFIG.set(&wasm_config, || {
                smol::block_on(ex.run(async {
                    shutdown
                        .recv()
                        .await
                        .context("Receiving shutdown notification")
                }))
            })
        })
        .finish(|| {
            let wasm_config =
                WasmConfig::new(&engine, &module).context("Linking the wasm configuration blob")?;
            WASM_CONFIG.set(&wasm_config, || {
                smol::block_on(async {
                    // Prepare the clients
                    let mut tls_client_cfg =
                        rustls::ClientConfig::with_ciphersuites(&rustls::ALL_CIPHERSUITES);
                    // TODO: see for configuring persistence, for more performance?
                    tls_client_cfg
                        .dangerous()
                        .set_certificate_verifier(Arc::new(NoCertVerifier));
                    let connector = async_tls::TlsConnector::from(tls_client_cfg);
                    let client = smtp_client::Client::new(
                        async_std_resolver::resolver_from_system_conf()
                            .await
                            .context("Configuring a resolver from system configuration")?,
                        Arc::new(ClientConfig::new(connector)),
                    );

                    // Spawn the queue
                    let storage = FsStorage::new(Arc::new(PathBuf::from(QUEUE_DIR)))
                        .await
                        .context("Opening the queue storage folder")?;
                    let queue = smtp_queue::Queue::new(
                        ex.clone(),
                        QueueConfig,
                        storage,
                        QueueTransport(client),
                    )
                    .await;

                    // Spawn the server
                    let tls_server_cfg = unblock(|| {
                        // Configure rustls
                        let mut tls_server_cfg = rustls::ServerConfig::with_ciphersuites(
                            rustls::NoClientAuth::new(),
                            &rustls::ALL_CIPHERSUITES,
                        );
                        // TODO: see for configuring persistence, for more performance?
                        // TODO: support SNI

                        // Load the certificates and keys
                        let cert = rustls::internal::pemfile::certs(&mut io::BufReader::new(
                            std::fs::File::open(CERT_FILE)
                                .context("Opening the certificate file")?,
                        ))
                        .map_err(|()| anyhow!("Failed parsing the certificate file"))?;
                        let keys =
                            rustls::internal::pemfile::pkcs8_private_keys(&mut io::BufReader::new(
                                std::fs::File::open(KEY_FILE).context("Opening the key file")?,
                            ))
                            .map_err(|()| anyhow!("Parsing the key file"))?;
                        anyhow::ensure!(keys.len() == 1, "Multiple keys found in the key file");
                        let key = keys.into_iter().next().unwrap();
                        tls_server_cfg
                            .set_single_cert(cert, key)
                            .context("Setting the key and certificate")?;

                        Ok(tls_server_cfg)
                    })
                    .await?;
                    let acceptor = async_tls::TlsAcceptor::from(tls_server_cfg);
                    let server_cfg = Arc::new(ServerConfig { acceptor, queue });
                    let listener = smol::net::TcpListener::bind("0.0.0.0:2525")
                        .await
                        .context("Binding on the listening port")?;
                    let mut incoming = listener.incoming();

                    info!("Server up, waiting for connections");
                    while let Some(stream) = incoming.next().await {
                        let stream = stream.context("Receiving a new incoming stream")?;
                        ex.spawn(smtp_server::interact(
                            stream,
                            smtp_server::IsAlreadyTls::No,
                            Vec::new(), // TODO
                            server_cfg.clone(),
                        ))
                        .detach();
                    }

                    // Close all the things
                    std::mem::drop(signal);

                    Ok(())
                })
            })
        });

    res
}