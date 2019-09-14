use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::io::Error as IoError;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use err_derive::Error;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use structopt::StructOpt;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, Sender};
use tokio::timer;

const TIMEOUT: Duration = Duration::from_secs(15);
static RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

#[derive(Debug, Error)]
#[error(display = "No samples")]
struct NoSamples;

/// The TCP spammer.
#[derive(StructOpt)]
enum Command {
    /// Run in server mode.
    ///
    /// In server mode, it accepts connections, reads the whole content and sends its own content
    /// back, then closes the connection.
    Server {
        /// The port to listen on.
        #[structopt(short = "p", long = "port", default_value = "2345")]
        port: u16,
        /// File with a content to send back. Nothing sent if missing.
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
    },
    /// Run in client mode, with provided rates to test with.
    ///
    /// Computes some statistics about latencies.
    Rate {
        /// The host to connect to.
        #[structopt(short = "h", long = "host", default_value = "127.0.0.1")]
        host: IpAddr,
        /// The port to connect to.
        #[structopt(short = "p", long = "port", default_value = "2345")]
        port: u16,
        /// The rate.
        ///
        /// Number of new connections started each second. Can be specified multiple times, in
        /// which case it'll run test with each.
        #[structopt(short = "r", long = "rate")]
        rate: Vec<u32>,
        /// The content to send to the server. Empty if missing.
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
        /// Length of each test, in seconds.
        #[structopt(short = "l", long = "length", default_value = "10")]
        length: u32,
    },
    /// Find the rate of connections at which the link saturates.
    ///
    /// Increases the rate of new connections in steps, until the last median is significantly
    /// worse than the previous step.
    Saturate {
        /// The host to connect to.
        #[structopt(short = "h", long = "host", default_value = "127.0.0.1")]
        host: IpAddr,
        /// The port to connect to.
        #[structopt(short = "p", long = "port", default_value = "2345")]
        port: u16,
        /// The rate to start at.
        #[structopt(short = "r", long = "start", default_value = "50")]
        start_rate: u32,
        /// Multiplication factor by which each step's rate is higher than the previous.
        #[structopt(short = "i", long = "increment", default_value = "1.25")]
        increment_factor: f64,
        /// A multiplication factor by which the current mean latency must be worse than the
        /// previous one to consider it a saturation.
        #[structopt(short = "s", long = "slowdown", default_value = "2")]
        slowdown_factor: f64,
        /// The content to send to the server.
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
        /// Length of each test, in seconds.
        #[structopt(short = "l", long = "length", default_value = "10")]
        length: u32,
    },
}

type Error = Box<dyn std::error::Error + Send + Sync>;

fn get_content(content: Option<PathBuf>) -> Result<Vec<u8>, Error> {
    Ok(content.map(fs::read).transpose()?.unwrap_or_default())
}

struct Sink;

impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, IoError>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context) -> Poll<Result<(), IoError>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context) -> Poll<Result<(), IoError>> {
        Poll::Ready(Ok(()))
    }
}

async fn handle_conn_inner(mut connection: TcpStream, content: Arc<[u8]>) -> Result<(), Error> {
    connection.copy(&mut Sink).await?;
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
    RUNTIME.block_on(async {
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
    connection.copy(&mut Sink).await?;
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

struct Latency {
    rate: u32,
    samples: usize,
    min: Duration,
    max: Duration,
    mean: Duration,
    p90: Duration,
}

impl Display for Latency {
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        write!(
            fmt,
            "Rate {} ({} samples):\tmin {:?},\tmax {:?},\tmean: {:?},\t90th: {:?}",
            self.rate, self.samples, self.min, self.max, self.mean, self.p90
        )
    }
}

fn run_rate(
    server: SocketAddr,
    rate: u32,
    cnt: u32,
    content: Arc<[u8]>,
) -> Result<Result<Latency, NoSamples>, Error> {
    RUNTIME.block_on(async {
        let (sender, receiver) = mpsc::channel(10);
        tokio::spawn(generator(server, rate, cnt, content, sender));
        // TODO: Better computation of stats
        let mut results: Vec<Duration> = receiver.collect().await;
        results.sort();
        if results.is_empty() {
            Ok(Err(NoSamples))
        } else {
            Ok(Ok(Latency {
                rate,
                samples: results.len(),
                min: results[0],
                max: results[results.len() - 1],
                mean: results[results.len() / 2],
                p90: results[results.len() * 9 / 10],
            }))
        }
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
            length,
        } => {
            let content = Arc::from(get_content(content)?);
            let sockaddr = SocketAddr::new(host, port);
            for rate in rate {
                info!("Running client to {} with rate {}", sockaddr, rate);
                let start = Instant::now();
                // TODO: Pauses in between
                match run_rate(sockaddr, rate, rate * length, Arc::clone(&content))? {
                    Ok(rate) => println!("{}", rate),
                    Err(NoSamples) => warn!("No samples for rate {}", rate),
                }
                info!("Step took {:?}", start.elapsed());
            }
        }
        Command::Saturate {
            host,
            port,
            start_rate,
            increment_factor,
            slowdown_factor,
            content,
            length,
        } => {
            let content = Arc::from(get_content(content)?);
            let sockaddr = SocketAddr::new(host, port);
            info!(
                "Running base client to {} with rate {}",
                sockaddr, start_rate
            );
            let mut prev = run_rate(
                sockaddr,
                start_rate,
                start_rate * length,
                Arc::clone(&content),
            )??;
            println!("Base {}", prev);
            for step in 2.. {
                let rate = (increment_factor.powi(step) * f64::from(start_rate)).round() as u32;
                info!("Running step {} with rate {}", step, rate);
                let start = Instant::now();
                let lat = run_rate(sockaddr, rate, rate * length, Arc::clone(&content))??;
                info!("Step {} took {:?}", step, start.elapsed());
                if lat.mean >= TIMEOUT
                    || prev.mean.as_secs_f64() * slowdown_factor < lat.mean.as_secs_f64()
                {
                    println!("Saturated at {}", lat);
                    break;
                } else {
                    println!("{}", lat);
                    prev = lat;
                }
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
