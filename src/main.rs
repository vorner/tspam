use std::fs;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use structopt::StructOpt;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, Sender};
use tokio::timer;

const TIMEOUT: Duration = Duration::from_secs(5);

/// The TCP spammer.
#[derive(StructOpt)]
enum Command {
    /// Run in server mode.
    ///
    /// In server mode, it accepts connections, reads the whole content and sends its own content
    /// back, then closes the connection.
    Server {
        /// The port to listen on.
        #[structopt(short = "p", long = "port")]
        port: u16,
        /// File with a content to send back. Nothing sent if missing.
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
    },
    Rate {
        #[structopt(short = "h", long = "host")]
        host: IpAddr,
        #[structopt(short = "p", long = "port")]
        port: u16,
        #[structopt(short = "r", long = "rate")]
        rate: Vec<u32>,
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
    },
}

type Error = Box<dyn std::error::Error + Send + Sync>;

fn get_content(content: Option<PathBuf>) -> Result<Vec<u8>, Error> {
    Ok(content.map(fs::read).transpose()?.unwrap_or_default())
}

async fn handle_conn_inner(mut connection: TcpStream, content: Arc<[u8]>) -> Result<(), Error> {
    let mut buffer = Vec::new();
    connection.read_to_end(&mut buffer).await?;
    connection.write_all(&content).await?;
    Ok(())
}

async fn handle_conn(addr: SocketAddr, connection: TcpStream, content: Arc<[u8]>) {
    if let Err(e) = handle_conn_inner(connection, content).await {
        error!("Error on connection {}: {}", addr, e);
    }
}

fn run_server(port: u16, content: Vec<u8>) -> Result<(), Error> {
    let content: Arc<[u8]> = Arc::from(content);
    let rt = Runtime::new()?;
    rt.block_on(async {
        let mut listener = TcpListener::bind((IpAddr::from(Ipv4Addr::UNSPECIFIED), port)).await?;
        loop {
            match listener.accept().await {
                Ok((connection, address)) => {
                    debug!("Accepted connection from {}", address);
                    tokio::spawn(handle_conn(address, connection, Arc::clone(&content)));
                }
                Err(e) => {
                    warn!("Failed to accept connection: {}", e);
                }
            }
        }
    })
}

async fn connect_inner(server: SocketAddr, content: Arc<[u8]>) -> Result<Duration, Error> {
    let start = Instant::now();
    let mut connection = TcpStream::connect(server).await?;
    connection.write_all(&content).await?;
    connection.shutdown(Shutdown::Write)?;
    let mut data = Vec::new();
    connection.read_to_end(&mut data).await?;
    Ok(start.elapsed())
}

async fn connect(server: SocketAddr, content: Arc<[u8]>, mut results: Sender<Duration>) {
    let connect = connect_inner(server, content).timeout(TIMEOUT);
    match connect.await {
        Ok(Ok(duration)) => results
            .send(duration)
            .await
            .expect("Channel prematurely closed"),
        Ok(Err(e)) => error!("Connection failed: {}", e),
        Err(_) => {
            warn!("Connection timed out");
            results
                .send(TIMEOUT)
                .await
                .expect("Channel prematurely closed");
        }
    }
}

async fn generator(
    server: SocketAddr,
    rate: u32,
    cnt: u32,
    content: Arc<[u8]>,
    results: Sender<Duration>,
) {
    debug!("Generator started");
    let interval = Duration::from_secs(1) / rate;
    let start = Instant::now();
    for i in 0..cnt {
        timer::delay(start + i * interval).await;
        debug!("Starting connection #{}", i);
        tokio::spawn(connect(server, Arc::clone(&content), results.clone()));
    }
    debug!("Generator terminated");
}

fn run_rate(server: SocketAddr, rate: u32, cnt: u32, content: Arc<[u8]>) -> Result<(), Error> {
    let rt = Runtime::new()?;
    rt.block_on(async {
        let (sender, receiver) = mpsc::channel(10);
        tokio::spawn(generator(server, rate, cnt, content, sender));
        // TODO: Better computation of stats
        let mut results: Vec<Duration> = receiver.collect().await;
        results.sort();
        if results.is_empty() {
            error!("No results for rate {}", rate);
        } else {
            // TODO: extract the part that runs the stuff, so we can base saturation on it.
            println!(
                "Rate {} ({} samples): min {:?}, max {:?}, mean: {:?}, 90th: {:?}",
                rate,
                results.len(),
                results[0],
                results[results.len() - 1],
                results[results.len() / 2],
                results[results.len() * 9 / 10]
            );
        }
        Ok(())
    })
}

fn run() -> Result<(), Error> {
    env_logger::init();
    let command = Command::from_args();
    match command {
        Command::Server { port, content } => {
            info!("Starting server on port {}", port);
            run_server(port, get_content(content)?)?;
        }
        Command::Rate {
            host,
            port,
            rate,
            content,
        } => {
            let content = Arc::from(get_content(content)?);
            let sockaddr = SocketAddr::new(host, port);
            for rate in rate {
                info!("Running client to {} with rate {}", sockaddr, rate);
                // TODO: Length
                // TODO: Pauses in between
                run_rate(sockaddr, rate, rate * 10, Arc::clone(&content))?;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        error!("Fatal error: {}", e);
        process::exit(1);
    }
}
