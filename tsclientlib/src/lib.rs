//! The base class of this library is the [`ConnectionManager`].
//!
//! [`ConnectionManager`]: struct.ConnectionManager.html

// TODO
#![allow(dead_code)]

extern crate base64;
extern crate chrono;
#[macro_use]
extern crate failure;
extern crate futures;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_perf;
extern crate slog_term;
extern crate tokio_core;
extern crate tomcrypt;
extern crate tsproto;
extern crate tsproto_commands;

use std::cell::{Ref, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;

use chrono::{DateTime, Duration, Utc};
use failure::{SyncFailure, ResultExt};
use futures::{future, Future, Sink, Stream};
use slog::{Drain, Logger};
use tokio_core::reactor::Handle;
use tsproto::algorithms as algs;
use tsproto::{client, packets, commands};
use tsproto::connectionmanager::ConnectionManager as TsprotoCM;
use tsproto::connectionmanager::{Resender, ResenderEvent};
use tsproto::packets::{Header, Packet, PacketType};
use tsproto_commands::*;
use tsproto_commands::messages::*;

// Reexports
pub use tsproto_commands::MoveReason;

macro_rules! copy_attrs {
    ($from:ident, $to:ident; $($attr:ident),* $(,)*; $($extra:ident: $ex:expr),* $(,)*) => {
        $to {
            $($attr: $from.$attr.clone(),)*
            $($extra: $ex,)*
        }
    };
}

macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(e) => e,
            Err(error) => return Box::new(future::err(error.into())),
        }
    };
}

mod structs;

type Result<T> = std::result::Result<T, Error>;
type BoxFuture<T> = Box<Future<Item = T, Error = Error>>;
type Map<K, V> = std::collections::HashMap<K, V>;

#[derive(Fail, Debug)]
pub enum Error {
    #[fail(display = "Connection failed ({})", _0)]
    ConnectionFailed(String),
    #[fail(display = "{}", _0)]
    Base64(#[cause] base64::DecodeError),
    #[fail(display = "{}", _0)]
    Tomcrypt(#[cause] SyncFailure<tomcrypt::errors::Error>),
    #[fail(display = "{}", _0)]
    Tsproto(tsproto::errors::Error),
    #[fail(display = "{}", _0)]
    Other(#[cause] failure::Compat<failure::Error>),
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::Base64(e)
    }
}

impl From<tomcrypt::errors::Error> for Error {
    fn from(e: tomcrypt::errors::Error) -> Self {
        Error::Tomcrypt(SyncFailure::new(e))
    }
}

impl From<tsproto::errors::Error> for Error {
    fn from(e: tsproto::errors::Error) -> Self {
        Error::Tsproto(SyncFailure::new(e))
    }
}

impl From<failure::Error> for Error {
    fn from(e: failure::Error) -> Self {
        let r: std::result::Result<(), _> = Err(e);
        Error::Other(r.compat().unwrap_err())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ChannelType {
    Permanent,
    SemiPermanent,
    Temporary,
}

include!(concat!(env!("OUT_DIR"), "/facades.rs"));

lazy_static! {
    /// Initialize `tsproto`
    static ref TSPROTO_INIT: () = tsproto::init()
        .expect("tsproto failed to initialize");
}

/// The connection manager which can be shared and cloned.
struct InnerCM {
    handle: Handle,
    logger: Logger,
    connections: Map<ConnectionId, structs::NetworkWrapper>,
}

impl InnerCM {
    /// Returns the first free connection id.
    fn find_connection_id(&self) -> ConnectionId {
        for i in 0..self.connections.len() + 1 {
            let id = ConnectionId(i);
            if !self.connections.contains_key(&id) {
                return id;
            }
        }
        unreachable!("Found no free connection id, this should not happen");
    }
}

/// The main type of this crate, which holds all connections.
///
/// It can be created with the [`ConnectionManager::new`] function:
///
/// ```
/// let core = tokio_core::Core::new()?;
/// let cm = ConnectionManager::new(core.handle());
/// ```
///
/// [`ConnectionManager::new`]: #method.new
pub struct ConnectionManager {
    inner: Rc<RefCell<InnerCM>>,
}

impl ConnectionManager {
    /// Creates a new `ConnectionManager` which is then used to add new
    /// connections.
    ///
    /// ```
    /// let core = tokio_core::Core::new()?;
    /// let cm = ConnectionManager::new(core.handle());
    /// ```
    ///
    /// Connecting to a server is done by [`ConnectionManager::add_connection`].
    ///
    /// [`ConnectionManager::add_connection`]: #method.add_connection
    pub fn new(handle: Handle) -> Self {
        // Initialize tsproto if it was not done yet
        *TSPROTO_INIT;

        // TODO Create with builder so the logger is optional
        // Don't log anything to console as default setting
        // Option to log to a file
        let logger = {
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator).build().fuse();
            let drain = slog_async::Async::new(drain).build().fuse();

            slog::Logger::root(drain, o!())
        };

        Self {
            inner: Rc::new(RefCell::new(InnerCM {
                handle,
                logger,
                connections: Map::new(),
            })),
        }
    }

    /// Connect to a server.
    pub fn add_connection(&mut self, mut config: ConnectOptions)
        -> BoxFuture<ConnectionId> {
        let inner = self.inner.borrow();
        let addr = config.address.expect(
            "Invalid ConnectOptions, this should not happen");
        let private_key = tryf!(config.private_key.take().map(|k| Ok(k))
            .unwrap_or_else(|| {
                // Create new ECDH key
                let prng = tomcrypt::sprng();
                tomcrypt::EccKey::new(prng, 32)
            }));

        let client = tryf!(client::ClientData::new(
            config.local_address,
            private_key,
            inner.handle.clone(),
            true,
            tsproto::connectionmanager::SocketConnectionManager::new(),
            None,
        ));

        // Set the data reference
        {
            let c2 = client.clone();
            let mut client = client.borrow_mut();
            client.connection_manager.set_data_ref(c2);
        }
        client::default_setup(client.clone(), false);

        // Create a connection
        let connect_fut = client::connect(client.clone(), addr);

        let logger = inner.logger.clone();
        let inner = Rc::downgrade(&self.inner);
        Box::new(connect_fut.map_err(|e| e.into()).and_then(move |()| {
            // TODO Add possibility to specify offset and level in ConnectOptions
            // Compute hash cash
            let mut time_reporter = slog_perf::TimeReporter::new_with_level(
                "Compute public key hash cash level", logger.clone(),
                slog::Level::Info);
            time_reporter.start("Compute public key hash cash level");
            let (offset, omega) = {
                let mut c = client.borrow_mut();
                (algs::hash_cash(&mut c.private_key, 8).unwrap(),
                base64::encode(&c.private_key.export_public().unwrap()))
            };
            time_reporter.finish();
            info!(logger, "Computed hash cash level";
                "level" => algs::get_hash_cash_level(&omega, offset),
                "offset" => offset);

            // Create clientinit packet
            let header = Header::new(PacketType::Command);
            let mut command = commands::Command::new("clientinit");
            command.push("client_nickname", config.name);
            command.push("client_version", "3.1.6 [Build: 1502873983]");
            command.push("client_platform", "Linux");
            command.push("client_input_hardware", "1");
            command.push("client_output_hardware", "1");
            command.push("client_default_channel", "");
            command.push("client_default_channel_password", "");
            command.push("client_server_password", "");
            command.push("client_meta_data", "");
            command.push("client_version_sign", "o+l92HKfiUF+THx2rBsuNjj/S1QpxG1fd5o3Q7qtWxkviR3LI3JeWyc26eTmoQoMTgI3jjHV7dCwHsK1BVu6Aw==");
            command.push("client_key_offset", offset.to_string());
            command.push("client_nickname_phonetic", "");
            command.push("client_default_token", "");
            command.push("hwid", "123,456");
            let p_data = packets::Data::Command(command);
            let clientinit_packet = Packet::new(header, p_data);

            let con = client.borrow().connection_manager
                .get_connection(addr).unwrap();
            let sink = client::ClientConnection::get_packets(con.clone());

            let client2 = client.clone();
            let con_weak = Rc::downgrade(&con);
            sink.send(clientinit_packet).and_then(move |_| {
                client::wait_until_connected(client2, addr)
            })
            .and_then(move |()| {
                // Wait for the initserver packet
                let stream = tsproto_commands::codec::CommandCodec::
                    new_stream_from_connection(con);
                stream.into_future().map_err(|(e, _)| e)
            }).map_err(|e| e.into())
            .and_then(move |(p, stream)| {
                if let Some(Notification::InitServer(p)) = p {
                    // Create a connection id
                    let inner = inner.upgrade().expect(
                        "Connection manager does not exist anymore");
                    let mut inner = inner.borrow_mut();
                    let id = inner.find_connection_id();

                    // Create the connection
                    let con = structs::NetworkWrapper::new(id, client, con_weak,
                        stream, p);

                    // Add the connection
                    inner.connections.insert(id, con);

                    // TODO spawn a future that polls the NetworkWrapper for this connection

                    future::ok(id)
                } else {
                    future::err(Error::ConnectionFailed(String::from(
                        "Got no initserver")))
                }
            })
        }))
    }

    /// Disconnect from a server.
    ///
    /// # Arguments
    /// - `id`: The connection which should be removed.
    /// - `options`: Either `None` or `DisconnectOptions`.
    ///
    /// # Examples
    ///
    /// Use default options:
    ///
    /// ```
    /// let mut core = tokio_core::Core::new()?;
    /// let mut cm = ConnectionManager::new(core.handle());
    ///
    /// // Add connection...
    ///
    /// # let con_id = ConnectionId(0);
    /// let disconnect_future = cm.remove_connection(con_id, None);
    /// core.run(disconnect_future)?;
    /// ```
    ///
    /// ```
    /// # let mut core = tokio_core::Core::new()?;
    /// # let mut cm = ConnectionManager::new(core.handle());
    /// # let con_id = ConnectionId(0);
    /// cm.remove_connection(con_id, DisconnectOptions::new()
    ///     .reason(MoveReason::Clientdisconnect)
    ///     .message("Away for a while"));
    /// ```
    pub fn remove_connection<O: Into<Option<DisconnectOptions>>>(&mut self,
        id: ConnectionId, options: O) -> BoxFuture<()> {
        let con = {
            let mut inner = self.inner.borrow_mut();
            if let Some(con) = inner.connections.remove(&id) {
                con
            } else {
                return Box::new(future::ok(()));
            }
        };
        let client_con = if let Some(c) = con.client_connection.upgrade() {
            c
        } else {
            // Already disconnected
            return Box::new(future::ok(()));
        };

        let header = Header::new(PacketType::Command);
        let mut command = commands::Command::new("clientdisconnect");

        // TODO use Notification for this
        let options = options.into().unwrap_or_default();
        if let Some(reason) = options.reason {
            command.push("reasonid", (reason as u8).to_string());
        }
        if let Some(msg) = options.message {
            command.push("reasonmsg", msg);
        }

        let p_data = packets::Data::Command(command);
        let packet = Packet::new(header, p_data);

        let addr;
        {
            let mut con = client_con.borrow_mut();
            con.resender.handle_event(ResenderEvent::Disconnecting);
            addr = con.address;
        }

        // TODO Remove connection here and also auto-remove on disconnect (e.g. kick)
        let sink = client::ClientConnection::get_packets(client_con);
        Box::new(sink.send(packet).and_then(move |_| {
            client::wait_for_state(con.client_data, addr, |state| {
                if let client::ServerConnectionState::Disconnected = *state {
                    true
                } else {
                    false
                }
            })
        }).map_err(|e| e.into()))
    }

    pub fn get_connection(&self, id: ConnectionId) -> Option<Connection> {
        if self.inner.borrow().connections.contains_key(&id) {
            Some(Connection { cm: &self, id })
        } else {
            None
        }
    }
}

// Private methods
impl ConnectionManager {
    fn get_server(&self, con: ConnectionId) -> Ref<structs::Server> {
        Ref::map(self.inner.borrow(), |r| &r.connections[&con].server)
    }
    fn get_optional_server_data(&self, con: ConnectionId) -> Ref<structs::OptionalServerData> {
        Ref::map(self.inner.borrow(), |r| r.connections[&con].server.optional_data.as_ref().unwrap())
    }
    fn get_connection_server_data(&self, con: ConnectionId) -> Ref<structs::ConnectionServerData> {
        Ref::map(self.inner.borrow(), |r| r.connections[&con].server.connection_data.as_ref().unwrap())
    }

    fn get_client(&self, con: ConnectionId, client: ClientId) -> Ref<structs::Client> {
        Ref::map(self.inner.borrow(), |r| &r.connections[&con].server.clients[&client])
    }
    fn get_optional_client_data(&self, con: ConnectionId, client: ClientId) -> Ref<structs::OptionalClientData> {
        Ref::map(self.inner.borrow(), |r| r.connections[&con].server.clients[&client].optional_data.as_ref().unwrap())
    }
    fn get_connection_client_data(&self, con: ConnectionId, client: ClientId) -> Ref<structs::ConnectionClientData> {
        Ref::map(self.inner.borrow(), |r| r.connections[&con].server.clients[&client].connection_data.as_ref().unwrap())
    }

    fn get_channel(&self, con: ConnectionId, chan: ChannelId) -> Ref<structs::Channel> {
        Ref::map(self.inner.borrow(), |r| &r.connections[&con].server.channels[&chan])
    }
    fn get_optional_channel_data(&self, con: ConnectionId, chan: ChannelId) -> Ref<structs::OptionalChannelData> {
        Ref::map(self.inner.borrow(), |r| r.connections[&con].server.channels[&chan].optional_data.as_ref().unwrap())
    }

    fn get_chat_entry(&self, _con: ConnectionId, _sender: ClientId) -> Ref<structs::ChatEntry> {
        unimplemented!("Chatting is not yet implemented")
    }
}

impl<'a> Connection<'a> {
    pub fn get_server(&self) -> Server {
        Server {
            cm: self.cm,
            connection_id: self.id,
        }
    }
}

/// The configuration used to create a new connection.
///
/// Basically, this is a builder for a connection.
///
/// # Example
///
/// ```
/// let addr: std::net::SocketAddr = "127.0.0.1:9987".parse().unwrap();
/// let con_config = ConnectOptions::from_address(addr);
///
/// let mut cm = ConnectionManager::new();
/// let con = cm.add_connection(con_config)?;
/// ```
#[derive(Debug)]
pub struct ConnectOptions {
    address: Option<SocketAddr>,
    local_address: SocketAddr,
    private_key: Option<tomcrypt::EccKey>,
    name: String,
}

impl ConnectOptions {
    /// A private method to create a config with only default values.
    ///
    /// This is not in the public interface because the created configuration
    /// is invalid.
    fn default() -> Self {
        Self {
            address: None,
            local_address: "0.0.0.0:0".parse().unwrap(),
            private_key: None,
            name: String::from("TeamSpeakUser"),
        }
    }

    /// Start creating the configuration of a new connection.
    ///
    /// The address of the server has to be supplied.
    pub fn from_address(address: SocketAddr) -> Self {
        Self {
            address: Some(address),
            .. Self::default()
        }
    }

    /// The address for the socket of our client
    ///
    /// # Default
    ///
    /// 0.0.0.0:0
    pub fn local_address(mut self, local_address: SocketAddr) -> Self {
        self.local_address = local_address;
        self
    }

    /// Set the private key of the user.
    ///
    /// # Default
    ///
    /// A new identity is generated when connecting.
    ///
    pub fn private_key_tomcrypt(mut self, private_key: tomcrypt::EccKey)
        -> Self {
        self.private_key = Some(private_key);
        self
    }

    /// Takes the private key as encoded by TeamSpeak (libtomcrypt export and
    /// base64 encoded).
    ///
    /// # Default
    ///
    /// A new identity is generated when connecting.
    ///
    /// # Error
    ///
    /// An error is returned if either the string is not encoded in valid base64
    /// or libtomcrypt cannot import the key.
    pub fn private_key_ts(mut self, private_key: &str) -> Result<Self> {
        self.private_key = Some(tomcrypt::EccKey::import(
            &base64::decode(private_key)?)?);
        Ok(self)
    }

    /// The name of the user.
    ///
    /// # Default
    ///
    /// TeamSpeakUser
    pub fn name(mut self, name: String) -> Self {
        self.name = name;
        self
    }
}

pub struct DisconnectOptions {
    reason: Option<MoveReason>,
    message: Option<String>,
}

impl Default for DisconnectOptions {
    fn default() -> Self {
        Self {
            reason: None,
            message: None,
        }
    }
}

impl DisconnectOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the reason for leaving.
    ///
    /// # Default
    ///
    /// None
    pub fn reason(mut self, reason: MoveReason) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Set the leave message.
    ///
    /// You also have to set the reason, otherwise the message will not be
    /// displayed.
    ///
    /// # Default
    ///
    /// None
    pub fn message<S: Into<String>>(mut self, message: S) -> Self {
        self.message = Some(message.into());
        self
    }
}
