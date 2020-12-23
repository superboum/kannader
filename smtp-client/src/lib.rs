use std::{cmp, collections::BTreeMap, future::Future, io, net::IpAddr, ops::Range, pin::Pin};

use async_trait::async_trait;
use chrono::Utc;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use rand::prelude::SliceRandom;
use smol::net::TcpStream;
use trust_dns_resolver::{error::ResolveError, proto::error::ProtoError, AsyncResolver, IntoName};

use smtp_message::{nom, Command, Email, EnhancedReplyCodeSubject, Hostname, Reply, ReplyCodeKind};

const SMTP_PORT: u16 = 25;

const RDBUF_SIZE: usize = 16 * 1024;
const MINIMUM_FREE_BUFSPACE: usize = 128;

pub struct Destination {
    host: Hostname,
}

#[async_trait]
pub trait Config {
    fn ehlo_hostname(&self) -> Hostname<&str>;

    fn can_do_tls(&self) -> bool {
        true
    }

    fn must_do_tls(&self) -> bool {
        false
    }

    /// Note: If this function can only fail, make can_do_tls return false
    async fn tls_connect<IO>(
        &self,
        io: IO,
    ) -> io::Result<
        duplexify::Duplex<Pin<Box<dyn Send + AsyncRead>>, Pin<Box<dyn Send + AsyncWrite>>>,
    >
    where
        IO: Send + AsyncRead + AsyncWrite;

    fn banner_read_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(5)
    }

    fn command_write_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(5)
    }

    fn ehlo_reply_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(5)
    }

    fn mail_reply_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(5)
    }

    fn rcpt_reply_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(5)
    }

    fn data_init_reply_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(2)
    }

    fn data_block_write_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(3)
    }

    fn data_end_reply_timeout(&self) -> chrono::Duration {
        chrono::Duration::minutes(10)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("Retrieving MX DNS records for ‘{1}’")]
    DnsMx(String, #[source] ResolveError),

    #[error("Converting hostname ‘{0}’ to to-be-resolved name")]
    HostToTrustDns(String, #[source] ProtoError),

    #[error("Retrieving IP DNS records for ‘{1}’")]
    DnsIp(trust_dns_resolver::Name, #[source] ResolveError),

    #[error("Connecting to ‘{0}’ port ‘{1}’")]
    Connecting(IpAddr, u16, #[source] io::Error),

    #[error("Receiving reply bytes")]
    ReceivingReplyBytes(#[source] io::Error),

    #[error("Timed out while waiting for a reply")]
    TimedOutWaitingForReply,

    #[error("Connection aborted")]
    ConnectionAborted,

    #[error("Reply does not fit in buffer: ‘{0}’")]
    TooLongReply(String),

    #[error("Syntax error parsing as a reply: ‘{0}’")]
    SyntaxError(String),

    #[error("Timed out while sending a command")]
    TimedOutSendingCommand,

    #[error("Sending command")]
    SendingCommand(#[source] io::Error),

    // TODO: add the command as error context
    #[error("Mail-level transient issue: {0}")]
    TransientMail(Reply<String>),

    #[error("Mailbox-level transient issue: {0}")]
    TransientMailbox(Reply<String>),

    #[error("Mail system-level transient issue: {0}")]
    TransientMailSystem(Reply<String>),

    #[error("Mail-level permanent issue: {0}")]
    PermanentMail(Reply<String>),

    #[error("Mailbox-level permanent issue: {0}")]
    PermanentMailbox(Reply<String>),

    #[error("Mail system-level permanent issue: {0}")]
    PermanentMailSystem(Reply<String>),

    #[error("Unexpected reply code: {0}")]
    UnexpectedReplyCode(Reply<String>),
}

pub enum TransportErrorSeverity {
    NetworkTransient,
    MailTransient,
    MailboxTransient,
    MailSystemTransient,
    MailPermanent,
    MailboxPermanent,
    MailSystemPermanent,
}

impl TransportError {
    pub fn severity(&self) -> TransportErrorSeverity {
        // TODO: Re-run over all these failure modes and check that the kind assignment
        // is correct. Maybe add categories like ProtocolPermanent for invalid
        // hostnames, or LocalTransient for local errors like “too many sockets opened”?
        match self {
            TransportError::DnsMx(_, _) => TransportErrorSeverity::NetworkTransient,
            TransportError::HostToTrustDns(_, _) => TransportErrorSeverity::NetworkTransient,
            TransportError::DnsIp(_, _) => TransportErrorSeverity::NetworkTransient,
            TransportError::Connecting(_, _, _) => TransportErrorSeverity::NetworkTransient,
            TransportError::ReceivingReplyBytes(_) => TransportErrorSeverity::NetworkTransient,
            TransportError::TimedOutWaitingForReply => TransportErrorSeverity::NetworkTransient,
            TransportError::ConnectionAborted => TransportErrorSeverity::NetworkTransient,
            TransportError::TooLongReply(_) => TransportErrorSeverity::NetworkTransient,
            TransportError::SyntaxError(_) => TransportErrorSeverity::MailSystemTransient,
            TransportError::TimedOutSendingCommand => TransportErrorSeverity::NetworkTransient,
            TransportError::SendingCommand(_) => TransportErrorSeverity::NetworkTransient,
            TransportError::TransientMail(_) => TransportErrorSeverity::MailTransient,
            TransportError::TransientMailbox(_) => TransportErrorSeverity::MailboxTransient,
            TransportError::TransientMailSystem(_) => TransportErrorSeverity::MailSystemTransient,
            TransportError::PermanentMail(_) => TransportErrorSeverity::MailPermanent,
            TransportError::PermanentMailbox(_) => TransportErrorSeverity::MailboxPermanent,
            TransportError::PermanentMailSystem(_) => TransportErrorSeverity::MailSystemPermanent,
            TransportError::UnexpectedReplyCode(_) => TransportErrorSeverity::NetworkTransient,
        }
    }
}

async fn read_for_reply<T>(
    fut: impl Future<Output = io::Result<T>>,
    waiting_for_reply_since: &chrono::DateTime<Utc>,
    timeout: chrono::Duration,
) -> Result<T, TransportError> {
    smol::future::or(
        async { fut.await.map_err(TransportError::ReceivingReplyBytes) },
        async {
            // TODO: this should be smol::Timer::at, but we would need to convert from
            // Chrono::DateTime<Utc> to std::time::Instant and I can't find how right now
            let max_delay: std::time::Duration = (*waiting_for_reply_since + timeout - Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(0));
            smol::Timer::after(max_delay).await;
            Err(TransportError::TimedOutWaitingForReply)
        },
    )
    .await
}

async fn read_reply<IO>(
    io: &mut IO,
    rdbuf: &mut [u8; RDBUF_SIZE],
    unhandled: &mut Range<usize>,
    timeout: chrono::Duration,
) -> Result<Reply<String>, TransportError>
where
    IO: Unpin + Send + AsyncRead + AsyncWrite,
{
    let start = Utc::now();
    // TODO: try to think of unifying this logic with the one in smtp-server?
    if (*unhandled).is_empty() {
        *unhandled = 0..read_for_reply(io.read(rdbuf), &start, timeout).await?;
        if (*unhandled).is_empty() {
            return Err(TransportError::ConnectionAborted);
        }
    }
    loop {
        match Reply::<&str>::parse(&rdbuf[unhandled.clone()]) {
            Err(nom::Err::Incomplete(n)) => {
                // Don't have enough data to handle command, let's fetch more
                if unhandled.start != 0 {
                    // Do we have to copy the data to the beginning of the buffer?
                    let missing = match n {
                        nom::Needed::Unknown => MINIMUM_FREE_BUFSPACE,
                        nom::Needed::Size(s) => cmp::max(MINIMUM_FREE_BUFSPACE, s.into()),
                    };
                    if missing > rdbuf.len() - unhandled.end {
                        rdbuf.copy_within(unhandled.clone(), 0);
                        unhandled.end = unhandled.len();
                        unhandled.start = 0;
                    }
                }
                if unhandled.end == rdbuf.len() {
                    // If we reach here, it means that unhandled is already
                    // basically the full buffer. Which means that we have to
                    // error out that the reply is too big.
                    // TODO: maybe there's something intelligent to be done here, like parsing reply
                    // line per reply line?
                    return Err(TransportError::TooLongReply(
                        String::from_utf8_lossy(&rdbuf[unhandled.clone()]).to_string(),
                    ));
                } else {
                    let read =
                        read_for_reply(io.read(&mut rdbuf[unhandled.end..]), &start, timeout)
                            .await?;
                    if read == 0 {
                        return Err(TransportError::ConnectionAborted);
                    }
                    unhandled.end += read;
                }
            }
            Err(_) => {
                // Syntax error
                // TODO: maybe we can recover better than this?
                return Err(TransportError::SyntaxError(
                    String::from_utf8_lossy(&rdbuf[unhandled.clone()]).to_string(),
                ));
            }
            Ok((rem, reply)) => {
                // Got a reply
                unhandled.start = unhandled.end - rem.len();
                // TODO: when polonius is ready, we can remove this allocation by returning a
                // borrow of the input buffer (with NLL it conflicts with the mutable borrow of
                // rdbuf in the other match arm)
                return Ok(reply.into_owned());
            }
        }
    }
}

fn verify_reply(r: Reply<String>, expected: ReplyCodeKind) -> Result<(), TransportError> {
    use EnhancedReplyCodeSubject::*;
    use ReplyCodeKind::*;
    use TransportError::*;
    match (r.code.kind(), r.ecode.as_ref().map(|e| e.subject())) {
        (k, _) if k == expected => Ok(()),
        (TransientNegative, Some(Mailbox)) => Err(TransientMailbox(r)),
        (PermanentNegative, Some(Mailbox)) => Err(PermanentMailbox(r)),
        (TransientNegative, Some(MailSystem)) => Err(TransientMailSystem(r)),
        (PermanentNegative, Some(MailSystem)) => Err(PermanentMailSystem(r)),
        (TransientNegative, _) => Err(TransientMail(r)),
        (PermanentNegative, _) => Err(PermanentMail(r)),
        (_, _) => Err(UnexpectedReplyCode(r)),
    }
}

async fn send_command<IO>(
    io: &mut IO,
    cmd: Command<&str>,
    timeout: chrono::Duration,
) -> Result<(), TransportError>
where
    IO: Unpin + Send + AsyncRead + AsyncWrite,
{
    smol::future::or(
        async {
            io.write_all_vectored(&mut cmd.as_io_slices().collect::<Vec<_>>())
                .await
                .map_err(TransportError::SendingCommand)?;
            Ok(())
        },
        async {
            smol::Timer::after(
                timeout
                    .to_std()
                    .unwrap_or(std::time::Duration::from_secs(0)),
            )
            .await;
            Err(TransportError::TimedOutSendingCommand)
        },
    )
    .await
}

pub struct Client<C, P, Cfg>
where
    C: trust_dns_resolver::proto::DnsHandle,
    P: trust_dns_resolver::ConnectionProvider<Conn = C>,
    Cfg: Config,
{
    resolver: AsyncResolver<C, P>,
    cfg: Cfg,
}

impl<C, P, Cfg> Client<C, P, Cfg>
where
    C: trust_dns_resolver::proto::DnsHandle,
    P: trust_dns_resolver::ConnectionProvider<Conn = C>,
    Cfg: Config,
{
    /// Note: Passing as `resolver` something that is configured with
    /// `Ipv6andIpv4` may lead to unexpected behavior, as the client will
    /// attempt to connect to both the Ipv6 and the Ipv4 address if whichever
    /// comes first doesn't successfully connect. In particular, it means that
    /// performance could be degraded.
    pub fn new(resolver: AsyncResolver<C, P>, cfg: Cfg) -> Client<C, P, Cfg> {
        Client { cfg, resolver }
    }

    pub async fn get_destination(&self, host: &Hostname) -> io::Result<Destination> {
        // TODO: already resolve here, but that means having to handle DNS expiration
        // down the road
        Ok(Destination { host: host.clone() })
    }

    pub async fn connect(&self, dest: &Destination) -> Result<Sender, TransportError> {
        match dest.host {
            Hostname::Ipv4 { ip, .. } => self.connect_to_ip(IpAddr::V4(ip), SMTP_PORT).await,
            Hostname::Ipv6 { ip, .. } => self.connect_to_ip(IpAddr::V6(ip), SMTP_PORT).await,
            Hostname::AsciiDomain { ref raw } => self.connect_to_mx(&raw).await,
            Hostname::Utf8Domain { ref punycode, .. } => self.connect_to_mx(&punycode).await,
        }
    }

    pub async fn connect_to_mx(&self, host: &str) -> Result<Sender, TransportError> {
        // TODO: consider adding a `.` at the end of `host`... but is it
        // actually allowed?
        // Run MX lookup
        let lookup = self
            .resolver
            .mx_lookup(host)
            .await
            .map_err(|e| TransportError::DnsMx(host.to_owned(), e))?;

        // Retrieve the actual records
        let mut mx_records = BTreeMap::new();
        for record in lookup.iter() {
            mx_records
                .entry(record.preference())
                .or_insert(Vec::with_capacity(1))
                .push(record.exchange());
        }

        // If there are no MX records, try A/AAAA records
        if mx_records.is_empty() {
            return self
                .connect_to_host(
                    host.into_name()
                        .map_err(|e| TransportError::HostToTrustDns(host.to_owned(), e))?,
                    SMTP_PORT,
                )
                .await;
        }

        // By increasing order of priority, try each MX
        // TODO: definitely should not return the first error but the first least severe
        // error
        let mut first_error = None;
        for (_, mut mxes) in mx_records {
            // Among a single priority level, randomize the order
            // TODO: consider giving a way to seed for reproducibility?
            mxes.shuffle(&mut rand::thread_rng());

            // Then try to connect to each address
            for mx in mxes {
                match self.connect_to_host(mx.clone(), SMTP_PORT).await {
                    Ok(sender) => return Ok(sender),
                    Err(e) => first_error = first_error.or(Some(e)),
                }
            }
        }

        // The below unwrap is safe because, to reach it:
        // - there must be some MX records or we'd have returned in the if above
        // - there have been no error as otherwise first_error wouldn't be None
        // - there must have only be errors as otherwise we'd have returned in the match
        //   above
        // Hence, if it triggers it means that \exists N, N > 1 \wedge N = 0, where N is
        // the number of errors.
        //   QED.
        Err(first_error.unwrap())
    }

    async fn connect_to_host(
        &self,
        name: trust_dns_resolver::Name,
        port: u16,
    ) -> Result<Sender, TransportError> {
        // Lookup the IP addresses associated with this name
        let lookup = self
            .resolver
            .lookup_ip(name.clone())
            .await
            .map_err(|e| TransportError::DnsIp(name, e))?;

        // Following the order given by the DNS server, attempt connecting
        // TODO: definitely should not return the first error but the first least severe
        // error
        let mut first_error = None;
        for ip in lookup.iter() {
            match self.connect_to_ip(ip, port).await {
                Ok(sender) => return Ok(sender),
                Err(e) => first_error = first_error.or(Some(e)),
            }
        }

        // See comment on connect_to_mx above for why this unwrap is correct
        Err(first_error.unwrap())
    }

    pub async fn connect_to_ip(&self, ip: IpAddr, port: u16) -> Result<Sender, TransportError> {
        let io = TcpStream::connect((ip, port))
            .await
            .map_err(|e| TransportError::Connecting(ip, port, e))?;
        let (reader, writer) = io.split();
        self.connect_to_stream(duplexify::Duplex::new(Box::pin(reader), Box::pin(writer)))
            .await
    }

    // TODO: add a connect_to_{host,ip}_smtps

    pub async fn connect_to_stream(
        &self,
        mut io: duplexify::Duplex<Pin<Box<dyn Send + AsyncRead>>, Pin<Box<dyn Send + AsyncWrite>>>,
    ) -> Result<Sender, TransportError> {
        // TODO: Are there interesting things to do with replies apart from checking
        // they're successful? Maybe logging them or something like that?
        let rdbuf = &mut [0; RDBUF_SIZE];
        let mut unhandled = 0..0;

        // Read the banner
        let reply = read_reply(
            &mut io,
            rdbuf,
            &mut unhandled,
            self.cfg.banner_read_timeout(),
        )
        .await?;
        verify_reply(reply, ReplyCodeKind::PositiveCompletion)?;

        // Send EHLO
        // TODO: fallback to HELO if EHLO fails (also record somewhere that this
        // destination doesn't support HELO)
        send_command(
            &mut io,
            Command::Ehlo {
                hostname: self.cfg.ehlo_hostname(),
            },
            self.cfg.command_write_timeout(),
        )
        .await?;
        let reply = read_reply(
            &mut io,
            rdbuf,
            &mut unhandled,
            self.cfg.ehlo_reply_timeout(),
        )
        .await?;
        verify_reply(reply, ReplyCodeKind::PositiveCompletion)?;

        // TODO: STARTTLS, AUTH... NOTE:
        // STARTTLS can fail, in which case we need to try reconnecting without
        // starttls
        Ok(Sender { io })
    }
}

pub struct Sender {
    io: duplexify::Duplex<Pin<Box<dyn Send + AsyncRead>>, Pin<Box<dyn Send + AsyncWrite>>>,
}

impl Sender {
    // TODO: Figure out a way to batch a single mail (with the same metadata) going
    // out to multiple recipients, so as to just use multiple RCPT TO
    pub async fn send<Reader>(
        &self,
        from: Option<&Email>,
        to: &Email,
        mail: Reader,
    ) -> Result<(), TransportError>
    where
        Reader: AsyncRead,
    {
        let _ = (from, to, mail, &self.io);
        todo!()
    }
}

// TODO: add tests
