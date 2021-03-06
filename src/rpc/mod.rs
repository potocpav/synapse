pub mod proto;
mod reader;
mod writer;
mod errors;
mod client;
mod processor;
mod transfer;

use std::{io, result, str, thread};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

use amy;
use serde_json;
use http_range::HttpRange;
use url::Url;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

pub use self::proto::resource;
pub use self::errors::{Error, ErrorKind, Result, ResultExt};
use self::proto::message::{self, SMessage};
use self::proto::ws;
use self::client::{Client, Incoming, IncomingStatus};
use self::processor::{Processor, TransferKind};
use self::transfer::{TransferResult, Transfers};
use bencode;
use handle;
use torrent;
use disk;
use socket::TSocket;
use util::UHashMap;
use CONFIG;

const POLL_INT_MS: usize = 1000;
const CLEANUP_INT_MS: usize = 2000;

lazy_static! {
    pub static ref EMPTY_HTTP_RESP: Vec<u8> = {
        let lines = vec![
            format!("HTTP/1.1 {} {}", 200, "OK"),
            format!("Connection: {}", "Close"),
            format!("Access-Control-Allow-Origin: {}", "*"),
            format!("Access-Control-Allow-Methods: {}", "OPTIONS, POST, GET"),
            format!("Accept-Ranges: {}", "bytes"),
            format!(
                "Access-Control-Allow-Headers: {}, {}, {}, {}, {}, {}, {}, {}",
                "Access-Control-Allow-Headers",
                "Origin",
                "Accept",
                "X-Requested-With",
                "Content-Type",
                "Access-Control-Request-Method",
                "Access-Control-Request-Headers",
                "Authorization"
            ),
            format!("\r\n"),
        ];
        lines.join("\r\n").into_bytes()
    };

    pub static ref UNAUTH_HTTP_RESP: Vec<u8> = {
        let lines = vec![
            format!("HTTP/1.1 {} {}", 401, "Unauthorized"),
            format!("Connection: {}", "Close"),
            format!("WWW-Authenticate: Basic real=m\"{}\"", "Synapse Connection"),
            format!("\r\n"),
        ];
        lines.join("\r\n").into_bytes()
    };

    pub static ref BAD_HTTP_RANGE: Vec<u8> = {
        let lines = vec![
            format!("HTTP/1.1 {} {}", 416, "Requested Range Not Satisfiable"),
            format!("Connection: {}", "Close"),
            format!("\r\n"),
        ];
        lines.join("\r\n").into_bytes()
    };
}

#[derive(Debug)]
pub enum CtlMessage {
    Extant(Vec<resource::Resource>),
    Update(Vec<resource::SResourceUpdate<'static>>),
    Removed(Vec<String>),
    ClientRemoved {
        id: String,
        client: usize,
        serial: u64,
    },
    Uploaded {
        id: String,
        client: usize,
        serial: u64,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum Message {
    UpdateTorrent(resource::CResourceUpdate),
    UpdateServer {
        id: String,
        throttle_up: Option<Option<i64>>,
        throttle_down: Option<Option<i64>>,
    },
    UpdateFile {
        id: String,
        torrent_id: String,
        priority: u8,
    },
    RemoveTorrent {
        id: String,
        client: usize,
        serial: u64,
        artifacts: bool,
    },
    Pause(String),
    Resume(String),
    Validate(Vec<String>),
    AddPeer {
        id: String,
        client: usize,
        serial: u64,
        peer: SocketAddr,
    },
    RemovePeer {
        id: String,
        torrent_id: String,
        client: usize,
        serial: u64,
    },
    AddTracker {
        id: String,
        client: usize,
        serial: u64,
        tracker: Url,
    },
    UpdateTracker {
        id: String,
        torrent_id: String,
    },
    RemoveTracker {
        id: String,
        torrent_id: String,
        client: usize,
        serial: u64,
    },
    Torrent {
        info: torrent::Info,
        client: usize,
        serial: u64,
        path: Option<String>,
        start: bool,
    },
}

#[allow(dead_code)]
pub struct RPC {
    poll: amy::Poller,
    reg: amy::Registrar,
    ch: handle::Handle<CtlMessage, Message>,
    listener: TcpListener,
    acceptor: Option<SslAcceptor>,
    lid: usize,
    cleanup: usize,
    processor: Processor,
    transfers: Transfers,
    clients: UHashMap<Client>,
    incoming: UHashMap<Incoming>,
    disk: amy::Sender<disk::Request>,
}

impl RPC {
    pub fn start(
        creg: &mut amy::Registrar,
        db: amy::Sender<disk::Request>,
    ) -> io::Result<(handle::Handle<Message, CtlMessage>, thread::JoinHandle<()>)> {
        let poll = amy::Poller::new()?;
        let mut reg = poll.get_registrar()?;
        let cleanup = reg.set_interval(CLEANUP_INT_MS)?;
        let (ch, dh) = handle::Handle::new(creg, &mut reg)?;

        let ip = if CONFIG.rpc.local {
            Ipv4Addr::new(127, 0, 0, 1)
        } else {
            Ipv4Addr::new(0, 0, 0, 0)
        };
        let port = CONFIG.rpc.port;
        let listener = TcpListener::bind(SocketAddrV4::new(ip, port))?;
        listener.set_nonblocking(true)?;
        let lid = reg.register(&listener, amy::Event::Both)?;

        let disk = db.try_clone()?;
        let th = dh.run("rpc", move |ch| {
            RPC {
                ch,
                disk,
                poll,
                reg,
                listener,
                lid,
                cleanup,
                clients: UHashMap::default(),
                incoming: UHashMap::default(),
                processor: Processor::new(db),
                transfers: Transfers::new(),
                acceptor: build_acceptor(&CONFIG.rpc.ssl_cert, &CONFIG.rpc.ssl_key),
            }.run()
        })?;
        Ok((ch, th))
    }

    pub fn run(&mut self) {
        debug!("Running RPC!");
        'outer: while let Ok(res) = self.poll.wait(POLL_INT_MS) {
            for not in res {
                match not.id {
                    id if id == self.lid => self.handle_accept(),
                    id if id == self.ch.rx.get_id() => {
                        if self.handle_ctl() {
                            return;
                        }
                    }
                    id if self.incoming.contains_key(&id) => self.handle_incoming(id),
                    id if id == self.cleanup => self.cleanup(),
                    id if self.transfers.contains(id) => self.handle_transfer(id),
                    _ => self.handle_conn(not),
                }
            }
        }
    }

    fn handle_ctl(&mut self) -> bool {
        while let Ok(m) = self.ch.recv() {
            match m {
                CtlMessage::Shutdown => return true,
                m => {
                    let msgs: Vec<_> = {
                        self.processor
                            .handle_ctl(m)
                            .into_iter()
                            .map(|(c, m)| (c, serde_json::to_string(&m).unwrap()))
                            .collect()
                    };
                    for (c, m) in msgs {
                        let res = match self.clients.get_mut(&c) {
                            Some(client) => client.send(ws::Frame::Text(m)),
                            None => {
                                debug!("Processor referenced a nonexistent client!");
                                Ok(())
                            }
                        };
                        if res.is_err() {
                            let client = self.clients.remove(&c).unwrap();
                            self.remove_client(c, client);
                        }
                    }
                }
            }
        }
        false
    }

    fn handle_transfer(&mut self, id: usize) {
        match self.transfers.ready(id) {
            TransferResult::Incomplete => {}
            TransferResult::Torrent {
                conn,
                data,
                path,
                client,
                serial,
                start,
            } => {
                debug!("Got torrent via HTTP transfer!");
                self.reg.deregister(&conn).unwrap();
                // TODO: Send this to the client in an error msg
                match bencode::decode_buf(&data) {
                    Ok(b) => match torrent::info::Info::from_bencode(b) {
                        Ok(i) => {
                            if self.ch
                                .send(Message::Torrent {
                                    info: i,
                                    path,
                                    start,
                                    client,
                                    serial,
                                })
                                .is_err()
                            {
                                error!("Failed to pass message to ctrl!");
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse torrent data: {}!", e);
                            self.clients.get_mut(&client).map(|c| {
                                c.send(ws::Frame::Text(
                                    serde_json::to_string(&SMessage::TransferFailed(
                                        message::Error {
                                            serial: Some(serial),
                                            reason: format!(
                                                "Invalid torrent file uploaded, {}.",
                                                e
                                            ),
                                        },
                                    )).unwrap(),
                                ))
                            });
                        }
                    },
                    Err(e) => {
                        error!("Failed to decode BE data: {}!", e);
                        self.clients.get_mut(&client).map(|c| {
                            c.send(ws::Frame::Text(
                                serde_json::to_string(&SMessage::TransferFailed(message::Error {
                                    serial: Some(serial),
                                    reason: format!(
                                        "Invalid torrent file uploaded, bad bencoded data: {}.",
                                        e
                                    ),
                                })).unwrap(),
                            ))
                        });
                    }
                }
            }
            TransferResult::Error {
                conn,
                err,
                client: id,
            } => {
                self.reg.deregister(&conn).unwrap();
                let res = self.clients
                    .get_mut(&id)
                    .map(|c| {
                        c.send(ws::Frame::Text(
                            serde_json::to_string(&SMessage::TransferFailed(err)).unwrap(),
                        ))
                    })
                    .unwrap_or(Ok(()));
                if res.is_err() {
                    let client = self.clients.remove(&id).unwrap();
                    self.remove_client(id, client);
                }
            }
        }
    }

    fn handle_accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((conn, ip)) => {
                    debug!("Accepted new connection from {:?}!", ip);
                    let id = self.reg.register(&conn, amy::Event::Both);
                    let conn = if let Some(ref acceptor) = self.acceptor {
                        TSocket::from_ssl(conn, acceptor)
                    } else {
                        TSocket::from_plain(conn)
                    };
                    if let (Ok(id), Ok(conn)) = (id, conn) {
                        self.incoming.insert(id, Incoming::new(conn));
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(e) => {
                    error!("Failed to accept conn: {}", e);
                }
            }
        }
    }

    fn handle_incoming(&mut self, id: usize) {
        if let Some(mut i) = self.incoming.remove(&id) {
            match i.readable() {
                Ok(IncomingStatus::Upgrade) => {
                    debug!("Succesfully upgraded conn");
                    self.clients.insert(id, i.into());
                }
                Ok(IncomingStatus::Incomplete) => {
                    self.incoming.insert(id, i);
                }
                Ok(IncomingStatus::Transfer { data, token }) => {
                    debug!("File transfer requested, validating");
                    match self.processor.get_transfer(token) {
                        Some((
                            client,
                            serial,
                            TransferKind::UploadTorrent { path, size, start },
                        )) => {
                            debug!("Torrent transfer initiated");
                            self.transfers.add_torrent(
                                id,
                                client,
                                serial,
                                i.into(),
                                data,
                                path,
                                size,
                                start,
                            );
                            // Since a succesful result means the buffer hasn't been flushed,
                            // immediatly attempt to handle the transfer as if it was ready
                            self.handle_transfer(id);
                        }
                        Some(_) => {
                            error!("Unimplemented transfer type ignored");
                        }
                        None => {
                            error!("Transfer used invalid token");
                            // TODO: Handle downloads and other uploads
                        }
                    }
                }
                Ok(IncomingStatus::DL { id, range }) => {
                    debug!("Attempting DL of {}", id);
                    let mut conn: TSocket = i.into();
                    if let Some((path, size)) = self.processor.get_dl(&id) {
                        if size == 0 {
                            conn.write(&EMPTY_HTTP_RESP).ok();
                            return;
                        }

                        let mut ranged = false;
                        let r = if let Some(r) = range {
                            if let Ok(ranges) = HttpRange::parse(&r, size) {
                                ranged = true;
                                ranges
                            } else {
                                debug!("Ranges {} invalid, stopping DL", id);
                                conn.write(&BAD_HTTP_RANGE).ok();
                                return;
                            }
                        } else {
                            vec![
                                HttpRange {
                                    start: 0,
                                    length: size,
                                },
                            ]
                        };
                        debug!("Initiating DL");
                        self.disk
                            .send(disk::Request::download(conn, path, r, ranged, size))
                            .ok();
                    } else {
                        debug!("ID {} invalid, stopping DL", id);
                        conn.write(&EMPTY_HTTP_RESP).ok();
                    }
                }
                Err(e) => {
                    debug!("Incoming ws upgrade failed: {}", e);
                }
            }
        }
    }

    fn handle_conn(&mut self, not: amy::Notification) {
        if let Some(mut c) = self.clients.remove(&not.id) {
            if not.event.readable() {
                loop {
                    match c.read() {
                        Ok(None) => break,
                        Ok(Some(ws::Frame::Text(data))) => {
                            if self.process_frame(not.id, &mut c, &data).is_err() {
                                debug!("Client error, disconnecting");
                                self.remove_client(not.id, c);
                                return;
                            }
                        }
                        Err(Error(ErrorKind::Complete, _)) => {
                            info!("Client disconnected");
                            self.remove_client(not.id, c);
                            return;
                        }
                        Ok(_) | Err(_) => {
                            debug!("Client error, disconnecting");
                            self.remove_client(not.id, c);
                            return;
                        }
                    }
                }
            }
            if not.event.writable() && c.write().is_err() {
                self.remove_client(not.id, c);
                return;
            }
            self.clients.insert(not.id, c);
        }
    }

    fn process_frame(&mut self, id: usize, c: &mut Client, data: &str) -> result::Result<(), ()> {
        match serde_json::from_str(data) {
            Ok(m) => {
                let (msgs, rm) = self.processor.handle_client(id, m);
                if let Some(m) = rm {
                    self.ch.send(m).unwrap();
                }
                for msg in msgs {
                    if c.send(ws::Frame::Text(serde_json::to_string(&msg).unwrap()))
                        .is_err()
                    {
                        return Err(());
                    }
                }
            }
            Err(e) => {
                if e.is_syntax() || e.is_eof() {
                    let msg = SMessage::InvalidSchema(message::Error {
                        serial: None,
                        reason: format!("JSON decode error: {}", e),
                    });
                    if c.send(ws::Frame::Text(serde_json::to_string(&msg).unwrap()))
                        .is_err()
                    {}
                    return Err(());
                }
                if e.is_data() {
                    #[derive(Deserialize)]
                    struct Serial {
                        serial: u64,
                    }

                    let serial = match serde_json::from_str::<Serial>(data) {
                        Ok(s) => Some(s.serial),
                        Err(_) => None,
                    };

                    let msg = SMessage::InvalidSchema(message::Error {
                        serial,
                        reason: format!("Invalid message format: {}", e),
                    });
                    if c.send(ws::Frame::Text(serde_json::to_string(&msg).unwrap()))
                        .is_err()
                    {
                        return Err(());
                    }
                }
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        self.processor.remove_expired_tokens();
        let reg = &self.reg;
        let processor = &mut self.processor;
        self.clients.retain(|id, client| {
            let res = client.timed_out();
            if res {
                info!("client {} timed out", id);
                reg.deregister(&client.conn).unwrap();
                processor.remove_client(*id);
            }
            !res
        });
        self.incoming.retain(|_, inc| {
            let res = inc.timed_out();
            if res {
                reg.deregister(&inc.conn).unwrap();
            }
            !res
        });
        for (conn, id, err) in self.transfers.cleanup() {
            reg.deregister(&conn).unwrap();
            self.clients.get_mut(&id).map(|c| {
                c.send(ws::Frame::Text(
                    serde_json::to_string(&SMessage::TransferFailed(err)).unwrap(),
                ))
            });
        }
    }

    fn remove_client(&mut self, id: usize, _client: Client) {
        self.processor.remove_client(id);
    }
}

fn build_acceptor(cert_file: &str, key_file: &str) -> Option<SslAcceptor> {
    if cert_file == "" || key_file == "" {
        info!("RPC SSL parameters not specified, using insecure connections!");
        return None;
    }

    let mut builder = match SslAcceptor::mozilla_intermediate(SslMethod::tls()) {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to create SSL acceptor: {}", e);
            return None;
        }
    };
    if let Err(e) = builder.set_certificate_chain_file(cert_file) {
        error!("Failed to load SSl certificate chain: {}", e);
        return None;
    }
    if let Err(e) = builder.set_private_key_file(key_file, SslFiletype::PEM) {
        error!("Failed to load SSl key: {}", e);
        return None;
    }
    if let Err(e) = builder.check_private_key() {
        error!("Failed to valdiate SSl key: {}", e);
        return None;
    }
    info!("SSL initialized!");
    Some(builder.build())
}
