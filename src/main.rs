use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::io::Error as IoError;
use std::mem;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use err_derive::Error;
use futures::future;
use log::{debug, error, info, warn};
use net2::unix::UnixTcpBuilderExt;
use net2::TcpBuilder;
use once_cell::sync::Lazy;
use structopt::StructOpt;
use tokio::io::{self, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{self, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
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
        /// The address to listen on.
        #[structopt(short = "l", long = "listen", default_value = "0.0.0.0")]
        listen: IpAddr,
        /// The port to listen on.
        #[structopt(short = "p", long = "port", default_value = "2345")]
        port: u16,
        /// File with a content to send back. Nothing sent if missing.
        #[structopt(short = "c", long = "content", parse(from_os_str))]
        content: Option<PathBuf>,
        /// Number of listen sockets and tasks.
        ///
        /// Raising this might be needed in high-performance scenarios.
        #[structopt(short = "a", long = "acceptors", default_value = "1")]
        acceptors: usize,
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
        /// Cooldown time (s) between tests,
        #[structopt(short = "o", long = "cooldown", default_value = "0")]
        cooldown: u64,
        /// Source IP to use.
        ///
        /// Can be used multiple times. If so, it is used in a round-robin fashion. If none
        /// provided, it is left up to the OS to decide.
        #[structopt(short = "I", long = "local-ip")]
        local_ips: Vec<IpAddr>,
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
        /// A cooldown time (s) in between two tests.
        #[structopt(short = "o", long = "cooldown", default_value = "0")]
        cooldown: u64,
        /// Source IP to use.
        ///
        /// Can be used multiple times. If so, it is used in a round-robin fashion. If none
        /// provided, it is left up to the OS to decide.
        #[structopt(short = "I", long = "local-ip")]
        local_ips: Vec<IpAddr>,
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
    io::copy(&mut connection, &mut Sink).await?;
    connection.write_all(&content).await?;
    Ok(())
}

async fn handle_conn(addr: SocketAddr, connection: TcpStream, content: Arc<[u8]>) {
    if let Err(e) = handle_conn_inner(connection, content).await {
        error!("Error on connection {}: {}", addr, e);
    }
}

fn run_server(listen: IpAddr, port: u16, acceptors: usize, content: Vec<u8>) -> Result<(), Error> {
    let content: Arc<[u8]> = Arc::from(content);
    RUNTIME.block_on(async {
        for _ in 0..acceptors {
            let listener = match listen {
                IpAddr::V4(_) => TcpBuilder::new_v4()?,
                IpAddr::V6(_) => TcpBuilder::new_v6()?,
            };
            if acceptors > 1 {
                listener.reuse_port(true)?;
            }
            let listener = listener
                .reuse_address(true)?
                .bind((listen, port))?
                .listen(20480)?;
            let listener = TcpListener::from_std(listener)?;
            let content = Arc::clone(&content);
            tokio::spawn(async move {
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
            });
        }
        future::pending().await
    })
}

async fn connect_inner(
    server: SocketAddr,
    content: Arc<[u8]>,
    _ip: Option<IpAddr>,
) -> Result<Duration, Error> {
    let start = Instant::now();
    let mut connection = TcpStream::connect(server).await?;
    /*
     * FIXME: The async connect with bound local address doesn't seem to be available? :-(
    let mut connection = match ip {
        None => TcpStream::connect(server).await?,
        Some(IpAddr::V4(addr)) => {
            let stream = TcpBuilder::new_v4()?.bind((addr, 0))?.to_tcp_stream()?;
            TcpStream::connect_std(stream, &server, &Default::default()).await?
        }
        Some(IpAddr::V6(addr)) => {
            let stream = TcpBuilder::new_v6()?.bind((addr, 0))?.to_tcp_stream()?;
            TcpStream::connect_std(stream, &server, &Default::default()).await?
        }
    };
    */
    connection.write_all(&content).await?;
    connection.shutdown().await?;
    io::copy(&mut connection, &mut Sink).await?;
    Ok(start.elapsed())
}

async fn connect(
    server: SocketAddr,
    content: Arc<[u8]>,
    ip: Option<IpAddr>,
    results: Sender<Duration>,
) {
    let connect = time::timeout(TIMEOUT, connect_inner(server, content, ip));
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
    local_ips: Arc<[IpAddr]>,
    results: Sender<Duration>,
) {
    debug!("Generator started");
    let interval = Duration::from_secs(1) / rate;
    let start = Instant::now();
    for i in 0..cnt {
        time::sleep_until(start + i * interval).await;
        debug!("Starting connection #{}", i);
        let ip = if local_ips.is_empty() {
            None
        } else {
            local_ips.get((i as usize) % local_ips.len()).cloned()
        };
        tokio::spawn(connect(server, Arc::clone(&content), ip, results.clone()));
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
    local_ips: Arc<[IpAddr]>,
) -> Result<Result<Latency, NoSamples>, Error> {
    RUNTIME.block_on(async {
        let (sender, mut receiver) = mpsc::channel(10);
        tokio::spawn(generator(server, rate, cnt, content, local_ips, sender));
        // TODO: Better computation of stats
        let mut results = Vec::new();
        while let Some(duration) = receiver.recv().await {
            results.push(duration);
        }
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
        Command::Server {
            listen,
            port,
            content,
            acceptors,
        } => {
            info!("Starting server on port {}", port);
            run_server(listen, port, acceptors, get_content(content)?)?;
        }
        Command::Rate {
            host,
            port,
            rate,
            content,
            length,
            cooldown,
            local_ips,
        } => {
            let content = Arc::from(get_content(content)?);
            let local_ips = Arc::from(local_ips);
            let sockaddr = SocketAddr::new(host, port);
            let cooldown = Duration::from_secs(cooldown);
            let mut first = true;
            for rate in rate {
                if !mem::replace(&mut first, false) {
                    thread::sleep(cooldown);
                }
                info!("Running client to {} with rate {}", sockaddr, rate);
                let start = Instant::now();
                // TODO: Pauses in between
                match run_rate(
                    sockaddr,
                    rate,
                    rate * length,
                    Arc::clone(&content),
                    Arc::clone(&local_ips),
                )? {
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
            cooldown,
            local_ips,
        } => {
            let content = Arc::from(get_content(content)?);
            let local_ips = Arc::from(local_ips);
            let sockaddr = SocketAddr::new(host, port);
            let cooldown = Duration::from_secs(cooldown);
            info!(
                "Running base client to {} with rate {}",
                sockaddr, start_rate
            );
            let mut prev = run_rate(
                sockaddr,
                start_rate,
                start_rate * length,
                Arc::clone(&content),
                Arc::clone(&local_ips),
            )??;
            println!("Base {}", prev);
            for step in 1.. {
                let rate = (increment_factor.powi(step) * f64::from(start_rate)).round() as u32;
                thread::sleep(cooldown);
                info!("Running step {} with rate {}", step, rate);
                let start = Instant::now();
                let lat = run_rate(
                    sockaddr,
                    rate,
                    rate * length,
                    Arc::clone(&content),
                    Arc::clone(&local_ips),
                )??;
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
