use futures::{StreamExt, SinkExt};
use warp::{Filter};
use warp::ws::{Ws, Message, WebSocket};

pub use tunnelto::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};

use futures::stream::{SplitSink, SplitStream};
use futures::channel::mpsc::{unbounded, UnboundedSender, UnboundedReceiver};
use lazy_static::lazy_static;
use log::{info, error};

mod connected_clients;
use self::connected_clients::*;
mod active_stream;
use self::active_stream::*;

mod remote;
mod control_server;

lazy_static! {
    pub static ref CONNECTIONS:Connections = Connections::new();
    pub static ref ACTIVE_STREAMS:ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
    pub static ref SECRET_KEY:SecretKey = load_secret_key();
    pub static ref ALLOWED_HOSTS:Vec<String> = allowed_host_suffixes();
}

/// TODO: add support for client registration and per-client api keys
/// For now this admin key is only for locking down custom deployments
/// See `allow_non_authenticated` for more.
pub fn load_secret_key() -> SecretKey {
    match std::env::var("SECRET_KEY") {
        Ok(key) => SecretKey(key),
        Err(_) => {
            eprintln!("Missing SECRET_KEY env, generating a new one,");
            SecretKey::generate()
        }
    }
}

/// What hosts do we allow tunnels on:
/// i.e:    baz.com => *.baz.com
///         foo.bar => *.foo.bar
pub fn allowed_host_suffixes() -> Vec<String> {
    std::env::var("ALLOWED_HOSTS")
        .map(|s| s.split(",").map(String::from).collect())
        .unwrap_or(vec![])
}

/// For demo purposes, allow unknown client connections
/// controlled by an env below
pub fn allow_unknown_clients() -> bool {
    std::env::var("ALLOW_UNKNOWN_CLIENTS").is_ok()
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    info!("starting wormhole server");

    control_server::spawn(([0,0,0,0], 5000));

    // create our accept any server
    let mut listener = TcpListener::bind("0.0.0.0:8080").await.expect("failed to bind");

    loop {
        let socket = match listener.accept().await {
            Ok((socket, _)) => socket,
            _ => {
                error!("failed to accept socket");
                continue;
            }
        };

        tokio::spawn(async move {
            remote::accept_connection(socket).await;
        });
    }
}