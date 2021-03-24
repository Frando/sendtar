use async_std::net::*;
use async_std::stream::StreamExt;
use async_std::task;
use async_tar::{Archive, Builder};
use clap::Clap;
use hypercore_protocol::{Channel, Event, Extension, Protocol, ProtocolBuilder};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

pub type Key = [u8; 32];

#[derive(Clap, Debug)]
pub struct Opts {
    #[clap(subcommand)]
    /// Mode (connect or accept)
    mode: Mode,
}

#[derive(Clap, Debug)]
enum Mode {
    /// Accept connections and print a token
    Recv(AcceptOpts),
    /// Connect via a token
    Send(ConnectOpts),
}

#[derive(Clap, Debug)]
pub struct AcceptOpts {
    /// File path to read from or write to.
    path: String,
}

#[derive(Clap, Debug)]
pub struct ConnectOpts {
    /// File path to read from or write to.
    path: String,
    /// Connection token
    token: String,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let opts: Opts = Opts::parse();
    // eprintln!("opts {:?}", opts);
    let res = task::block_on(run(opts));

    if let Err(e) = res {
        eprintln!("{}", e);
        std::process::exit(1);
    }
    Ok(())
}

pub async fn run(opts: Opts) -> anyhow::Result<()> {
    match opts.mode {
        Mode::Recv(opts) => run_accept(opts).await,
        Mode::Send(opts) => run_connect(opts).await,
    }
}

pub fn not_empty_and_exists(path: &str) -> io::Result<()> {
    let is_empty = PathBuf::from(&path)
        .read_dir()
        .map(|mut i| i.next().is_none())
        .unwrap_or(false);
    if !is_empty {
        Err(io_err("Directory is not empty or does not exist").into())
    } else {
        Ok(())
    }
}

pub async fn run_connect(opts: ConnectOpts) -> anyhow::Result<()> {
    eprintln!("send folder {}", opts.path);
    let token = Token::decode(&opts.token)?;
    let stream = connect_one(token.addrs, token.port).await?;
    eprintln!("new connection to {}", stream.local_addr()?);
    let channel = get_channel(stream, token.secret, true).await?;
    on_client_channel(channel, opts.path).await?;
    Ok(())
}

pub async fn run_accept(opts: AcceptOpts) -> anyhow::Result<()> {
    fs::create_dir_all(&opts.path)?;
    not_empty_and_exists(&opts.path)?;
    let addrs = find_addresses()?;
    let secret: [u8; 32] = rand::random();
    // let secret: [u8; 32] = vec![1u8; 32].try_into().unwrap();
    let port = 0;
    // let port = 10000;

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();

    let token = Token::new(addrs, port, secret);
    eprintln!("write into folder {}", &opts.path);
    eprintln!("token to copy to peer:");
    eprintln!("{}", token.encode());

    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        eprintln!("new connection from {}", stream.local_addr()?);
        let channel = get_channel(stream, secret, false).await?;
        on_server_channel(channel, opts.path.clone()).await?;
    }

    Ok(())
}

pub async fn connect_one(addrs: Vec<IpAddr>, port: u16) -> io::Result<TcpStream> {
    for addr in addrs.iter() {
        let addr = SocketAddr::new(*addr, port);
        let stream = TcpStream::connect(addr).await;
        if stream.is_ok() {
            return stream;
        }
    }
    return Err(io::Error::new(
        io::ErrorKind::Other,
        "Could not connect to any address",
    ));
}

pub async fn get_channel(
    stream: TcpStream,
    secret: Key,
    is_initiator: bool,
) -> io::Result<Extension> {
    let mut proto = ProtocolBuilder::new(is_initiator).connect(stream);
    proto.open(secret).await?;
    let mut ch = drive_until_channel(&mut proto).await?;
    let ext = open_extension(&mut ch).await?;
    drive_protocol(proto, ch);
    Ok(ext)
}

pub async fn drive_until_channel(proto: &mut Protocol<TcpStream>) -> io::Result<Channel> {
    while let Some(event) = proto.next().await {
        match event {
            Ok(Event::Channel(channel)) => {
                // eprintln!("found channel! {:?}", channel);
                return Ok(channel);
            }
            _ => {}
        }
    }
    return Err(io::Error::new(io::ErrorKind::Other, "No channel open"));
}

pub fn drive_protocol(mut proto: Protocol<TcpStream>, mut channel: Channel) {
    task::spawn(async move { while let Some(_event) = channel.next().await {} });
    task::spawn(async move {
        while let Some(event) = proto.next().await {
            match event {
                Err(e) => eprintln!("Error: {}", e),
                Ok(Event::Channel(_channel)) => {
                    // eprintln!("found channel! {:?}", channel);
                }
                _ => {}
            }
        }
    });
}

pub async fn on_server_channel(ext: Extension, path: String) -> io::Result<()> {
    eprintln!("write into dir: {}", path);
    let archive = Archive::new(ext);
    archive.unpack(&path).await?;
    eprintln!("done");
    Ok(())
}

pub async fn on_client_channel(ext: Extension, path: String) -> io::Result<()> {
    eprintln!("read from dir: {}", path);
    let mut archive = Builder::new(ext);
    archive.append_dir_all(".", &path).await?;
    archive.finish().await?;
    eprintln!("done");
    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Token {
    addrs: Vec<IpAddr>,
    secret: Key,
    port: u16,
}

impl Token {
    pub fn new(addrs: Vec<IpAddr>, port: u16, secret: Key) -> Self {
        Self {
            addrs,
            secret,
            port,
        }
    }
}

impl Token {
    pub fn decode(msg: &str) -> io::Result<Self> {
        let buf = base32::decode(base32::Alphabet::Crockford, &msg.to_uppercase()).unwrap();
        let this: Self = bincode::deserialize(&buf).unwrap();
        Ok(this)
    }

    pub fn encode(&self) -> String {
        let encoded: Vec<u8> = bincode::serialize(&self).unwrap();
        base32::encode(base32::Alphabet::Crockford, &encoded)
            .to_lowercase()
            .to_string()
    }
}

pub async fn open_extension(channel: &mut Channel) -> io::Result<Extension> {
    let ext = channel.register_extension("sendtar").await;
    Ok(ext)
}

fn io_err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

pub fn find_addresses() -> io::Result<Vec<IpAddr>> {
    let ifaces = get_if_addrs::get_if_addrs()?;

    let mut addrs = vec![];
    for iface in ifaces {
        if !iface.addr.ip().is_loopback() && iface.addr.ip().is_ipv4() {
            addrs.push(iface.addr.ip());
        }
    }
    Ok(addrs)
}
