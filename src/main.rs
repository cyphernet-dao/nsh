#[macro_use]
extern crate amplify;

use std::any::Any;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::{fs, io, thread};

use clap::Parser;
use cyphernet::addr::{HostName, InetHost, Localhost, NetAddr, PartialAddr, PeerAddr};
use cyphernet::{ed25519, Cert, Digest, EcPk, EcSign, EcSk, Sha256};
use netservices::tunnel::Tunnel;
use netservices::NetSession;
use nsh::client::Client;
use nsh::command::Command;
use nsh::processor::Processor;
use nsh::server::{Accept, Server};
use nsh::shell::LogLevel;
use nsh::{RemoteHost, Session, Transport};
use reactor::poller::popol;
use reactor::Reactor;

pub const DEFAULT_PORT: u16 = 3232;
pub const DEFAULT_SOCKS5_PORT: u16 = 9050; // We default to Tor proxy

pub const DEFAULT_DIR: &'static str = "~/.nsh";
pub const DEFAULT_ID_FILE: &'static str = "ssi_ed25519";

type AddrArg = PartialAddr<HostName, DEFAULT_PORT>;

#[derive(Clone, Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Verbosity level
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Start as a daemon listening on a specific socket
    ///
    /// If the socket address is not given, defaults to 127.0.0.1:3232
    #[arg(short, long)]
    pub listen: Option<Option<PartialAddr<InetHost, DEFAULT_PORT>>>,

    /// Path to an identity (key) file
    #[arg(short, long, require_equals = true)]
    pub id: Option<PathBuf>,

    /// SOCKS5 proxy, as IPv4 or IPv6 socket
    ///
    /// If port is not given, defaults to 9050.
    #[arg(short = 'p', long, conflicts_with = "listen", require_equals = true)]
    pub proxy: Option<Option<PartialAddr<InetHost, DEFAULT_SOCKS5_PORT>>>,

    /// Tunneling mode
    ///
    /// In tunnel mode listens on a provided address and tunnels all incoming
    /// connections to the `REMOTE_HOST`.
    ///
    /// If the socket address is not given, defaults to 127.0.0.1:3232
    #[arg(short, long, conflicts_with = "listen")]
    pub tunnel: Option<Option<PartialAddr<InetHost, DEFAULT_SOCKS5_PORT>>>,

    /// Address of the remote host to connect
    ///
    /// Remote address, if no proxy is used, should be either IPv4 or IPv6
    /// socket address with optional port information. If SOCKS5 proxy is used,
    /// (see `--socks5` argument) remote address can be a string representing
    /// address supported by the specific proxy, for instance Tor, I2P or
    /// Nym address.
    ///
    /// If the address is provided without a port, a default port 3232 is used.
    #[arg(conflicts_with = "listen", required_unless_present = "listen")]
    pub remote_host: Option<PeerAddr<ed25519::PublicKey, AddrArg>>,

    /// Connection timeout duration, in seconds
    #[arg(short = 'T', long, default_value = "10", require_equals = true)]
    pub timeout: u8,

    /// Command to execute on the remote host
    #[arg(conflicts_with_all = ["listen", "tunnel"], required_unless_present_any = ["listen", "tunnel"])]
    pub command: Option<Command>,
}

enum Mode {
    Listen(NetAddr<InetHost>),
    Tunnel {
        local: NetAddr<InetHost>,
        remote: RemoteHost,
    },
    Connect {
        host: RemoteHost,
        command: Command,
    },
}

#[derive(Getters, Clone, Eq, PartialEq)]
pub struct NodeKeys {
    pk: ed25519::PublicKey,
    sk: ed25519::PrivateKey,
    cert: Cert<ed25519::Signature>,
}

impl From<ed25519::PrivateKey> for NodeKeys {
    fn from(sk: ed25519::PrivateKey) -> Self {
        let pk = sk.to_pk().expect("invalid node private key");
        let cert = Cert {
            pk: pk.clone(),
            sig: sk.sign(pk.to_pk_compressed()),
        };
        NodeKeys { pk, sk, cert }
    }
}

struct Config {
    pub node_keys: NodeKeys,
    pub mode: Mode,
    pub force_proxy: bool,
    pub proxy_addr: NetAddr<InetHost>,
    pub timeout: Duration,
}

#[derive(Debug, Display, Error, From)]
#[display(inner)]
pub enum AppError {
    #[from]
    Io(io::Error),

    #[from]
    Curve25519(ec25519::Error),

    #[from]
    Reactor(reactor::Error<Accept, Transport>),

    //    #[from]
    //    Socks5(socks5::Error),
    #[from]
    #[display("error creating thread")]
    Thread(Box<dyn Any + Send>),

    #[display("unable to construct tunnel with {0}: {1}")]
    Tunnel(RemoteHost, io::Error),
}

impl TryFrom<Args> for Config {
    type Error = AppError;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        let command = if let Some(listen) = args.listen {
            let local_socket = listen.unwrap_or_else(Localhost::localhost).into();
            Mode::Listen(local_socket)
        } else if let Some(tunnel) = args.tunnel {
            let local = tunnel
                .unwrap_or_else(|| PartialAddr::localhost(None))
                .into();
            let remote = args.remote_host.expect("clap library broken");
            Mode::Tunnel {
                local,
                remote: remote.into(),
            }
        } else {
            let host = args.remote_host.expect("clap library broken");
            Mode::Connect {
                host: host.into(),
                command: args.command.unwrap_or(Command::DATE),
            }
        };

        let id_path = args.id.unwrap_or_else(|| {
            let mut dir = PathBuf::from(DEFAULT_DIR);
            dir.push(DEFAULT_ID_FILE);
            dir
        });
        let id_path = shellexpand::tilde(&id_path.to_string_lossy()).to_string();
        let id_pem = fs::read_to_string(&id_path).or_else(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                println!(
                    "Identity file not found; creating new identity in '{}'",
                    id_path
                );
                let pair = ec25519::KeyPair::generate();
                let pem = pair.sk.to_pem();
                let mut dir = PathBuf::from(&id_path);
                dir.pop();
                fs::create_dir_all(dir)?;
                fs::write(id_path, &pem)?;
                Ok(pem)
            } else {
                Err(err)
            }
        })?;
        let id_priv = ed25519::PrivateKey::from_pem(&id_pem)?;
        let node_keys = NodeKeys::from(id_priv);
        println!("Using identity {}", node_keys.pk());

        let force_proxy = args.proxy.is_some();
        let proxy_addr = args
            .proxy
            .flatten()
            .unwrap_or(Localhost::localhost())
            .into();

        Ok(Config {
            node_keys,
            mode: command,
            proxy_addr,
            force_proxy,
            timeout: Duration::from_secs(args.timeout as u64),
        })
    }
}

fn run() -> Result<(), AppError> {
    let args = Args::parse();

    LogLevel::from_verbosity_flag_count(args.verbose).apply();

    let config = Config::try_from(args)?;

    match config.mode {
        Mode::Listen(socket_addr) => {
            println!("Listening on {socket_addr} ...");

            let processor = Processor::with(
                config.node_keys.cert,
                config.node_keys.sk.clone(),
                config.proxy_addr,
                config.force_proxy,
                config.timeout,
            );
            let service = Server::with(&socket_addr, processor)?;
            let reactor = Reactor::with(
                service,
                popol::Poller::new(),
                thread::Builder::new().name(s!("reactor")),
            )?;

            reactor.join()?;
        }
        Mode::Tunnel { remote, local } => {
            eprintln!("Tunneling to {remote} from {local}...");

            let session = Session::connect_blocking::<{ Sha256::OUTPUT_LEN }>(
                remote.addr.clone(),
                config.node_keys.cert,
                vec![remote.id],
                config.node_keys.sk.clone(),
                config.proxy_addr.clone(),
                config.force_proxy,
                config.timeout,
            )?;
            let mut tunnel = match Tunnel::with(session, local) {
                Ok(tunnel) => tunnel,
                Err((session, err)) => {
                    session.disconnect()?;
                    return Err(AppError::Tunnel(remote, err));
                }
            };
            let _ = tunnel.tunnel_once(popol::Poller::new(), Duration::from_secs(10))?;
            tunnel.into_session().disconnect()?;
        }
        Mode::Connect { host, command } => {
            eprint!("Connecting to {host} ");
            if config.force_proxy {
                eprint!("using proxy {} ", config.proxy_addr);
            }
            eprintln!("...");

            let mut stdout = io::stdout();

            let mut client = Client::connect(
                host,
                config.node_keys.cert,
                config.node_keys.sk,
                config.proxy_addr,
                config.force_proxy,
                config.timeout,
            )?;
            let mut printout = client.exec(command)?;
            eprintln!("Remote output >>>");
            for batch in &mut printout {
                stdout.write_all(&batch)?;
            }
            stdout.flush()?;
            client = printout.complete();
            client.disconnect()?;

            eprintln!("<<< done");
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {}", err);
            ExitCode::FAILURE
        }
    }
}
