use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use async_std::io;
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::task;
use log::{debug, error, warn};
use structopt::StructOpt;

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
}

type Error = Box<dyn std::error::Error + Send + Sync>;

async fn handle_conn_inner(mut connection: TcpStream, content: Arc<[u8]>) -> Result<(), Error> {
    io::copy(&mut connection, &mut io::sink()).await?;
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
    task::block_on(async {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            match listener.accept().await {
                Ok((connection, address)) => {
                    debug!("Accepted connection from {}", address);
                    task::spawn(handle_conn(address, connection, Arc::clone(&content)));
                }
                Err(e) => {
                    warn!("Failed to accept connection: {}", e);
                }
            }
        }
    })
}

fn run() -> Result<(), Error> {
    let command = Command::from_args();
    match command {
        Command::Server { port, content } => {
            let content = content.map(fs::read).transpose()?.unwrap_or_default();
            run_server(port, content)?;
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
