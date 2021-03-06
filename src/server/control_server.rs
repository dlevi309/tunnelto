pub use super::*;
use std::net::SocketAddr;
use std::time::Duration;

pub fn spawn<A: Into<SocketAddr>>(addr: A) {
    let health_check = warp::get().and(warp::path("health_check")).map(|| "ok");
    let client_conn = warp::path("wormhole").and(warp::ws()).map(move |ws: Ws| {
        ws.on_upgrade(handle_new_connection)
    });

    // spawn our websocket control server
    tokio::spawn(warp::serve(client_conn.or(health_check)).run(addr.into()));
}

async fn handle_new_connection(websocket: WebSocket) {
    let (websocket, client_id, sub_domain) = match try_client_handshake(websocket).await {
        Some(ws) => ws,
        None => return,
    };

    let (tx, rx) = unbounded::<ControlPacket>();
    let client = ConnectedClient { id: client_id, host: sub_domain, tx };
    Connections::add(client.clone());

    let  (sink, stream) = websocket.split();

    let client_clone = client.clone();

    tokio::spawn(async move {
        tunnel_client(client_clone, sink, rx).await;
    });

    tokio::spawn(async move {
        process_client_messages(client, stream).await;
    });
}

async fn try_client_handshake(mut websocket: WebSocket) -> Option<(WebSocket, ClientId, String)> {
    // Wait for control hello
    let client_hello_data = match websocket.next().await {
        Some(Ok(msg)) => msg,
        _ => {
            error!("no client init message");
            return None
        },
    };

    let client_hello = ClientHello::verify(&SECRET_KEY, client_hello_data.as_bytes(), allow_unknown_clients())
        .map_err(|e| format!("{:?}", e));

    let (client_hello, sub_domain) = match  client_hello {
        Ok(ch) => {

            let sub_domain = match  &ch.sub_domain {
                None => ServerHello::random_domain(),

                // otherwise, try to assign the sub domain
                Some(sub_domain) => {
                    // ignore uppercase
                    let sub_domain = sub_domain.to_lowercase();

                    if sub_domain.chars().filter(|c| !c.is_alphanumeric()).count() > 0 {
                        error!("invalid client hello: only alphanumeric chars allowed!");
                        let data = serde_json::to_vec(&ServerHello::InvalidSubDomain).unwrap_or_default();
                        let _ = websocket.send(Message::binary(data)).await;
                        return None
                    }

                    // don't allow specified domains for anonymous clients
                    if ch.is_anonymous {
                        ServerHello::prefixed_random_domain(&sub_domain)
                    } else {
                        let existing_client = Connections::client_for_host(&sub_domain);
                        if existing_client.is_some() && Some(&ch.id) != existing_client.as_ref() {
                            error!("invalid client hello: requested sub domain in use already!");
                            let data = serde_json::to_vec(&ServerHello::SubDomainInUse).unwrap_or_default();
                            let _ = websocket.send(Message::binary(data)).await;
                            return None
                        }

                        sub_domain
                    }
                }
            };


            (ch, sub_domain)
        },
        Err(e) => {
            error!("invalid client hello: {}", e);
            let data = serde_json::to_vec(&ServerHello::AuthFailed).unwrap_or_default();
            let _ = websocket.send(Message::binary(data)).await;
            return None
        }
    };

    // Send server hello success
    let data = serde_json::to_vec(&ServerHello::Success { sub_domain: sub_domain.clone() }).unwrap_or_default();
    let send_result = websocket.send(Message::binary(data)).await;
    if let Err(e) = send_result {
        error!("aborting...failed to write server hello: {:?}", e);
        return None
    }

    info!("new client connected: {:?}{}", &client_hello.id, if client_hello.is_anonymous { " (anonymous)"} else { "" });
    Some((websocket, client_hello.id, sub_domain))
}

/// Send the client a "stream init" message
pub async fn send_client_stream_init(mut stream: ActiveStream) {
    match stream.client.tx.send(ControlPacket::Init(stream.id.clone())).await {
        Ok(_) => {
            info!("sent control to client: {}", &stream.client.id);
        },
        Err(_) => {
            info!("removing disconnected client: {}", &stream.client.id);
            Connections::remove(&stream.client);
        }
    }

}

/// Process client control messages
async fn process_client_messages(client: ConnectedClient, mut client_conn: SplitStream<WebSocket>) {
    loop {
        let result = client_conn.next().await;

        let message = match result {
            Some(Ok(msg)) if !msg.as_bytes().is_empty() => msg,
            _ => {
                info!("goodbye client: {:?}", &client.id);
                Connections::remove(&client);
                return
            },
        };

        let packet = match ControlPacket::deserialize(message.as_bytes()) {
            Ok(packet) => packet,
            Err(e) => {
                eprintln!("invalid data packet: {:?}", e);
                continue
            }
        };

        let (stream_id, message) = match packet {
            ControlPacket::Data(stream_id, data) => {
                info!("forwarding to stream[id={}]: {} bytes", &stream_id.to_string(), data.len());
                (stream_id, StreamMessage::Data(data))
            },
            ControlPacket::Refused(stream_id) => {
                log::info!("tunnel says: refused");
                (stream_id, StreamMessage::TunnelRefused)
            }
            ControlPacket::Init(_) | ControlPacket::End(_) => {
                error!("invalid protocol control::init message");
                continue
            },
            ControlPacket::Ping => {
                log::info!("got ping");

                let mut tx = client.tx.clone();
                tokio::spawn(async move {
                    tokio::time::delay_for(Duration::new(PING_INTERVAL, 0)).await;
                    let _ = tx.send(ControlPacket::Ping).await;
                });
                continue
            }
        };

        let stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();

        if let Some(mut stream) = stream {
            let _ = stream.tx.send(message).await.map_err(|e| {
                log::error!("Failed to send to stream tx: {:?}", e);
            });
        }
    }
}

async fn tunnel_client(client: ConnectedClient, mut sink: SplitSink<WebSocket, Message>, mut queue: UnboundedReceiver<ControlPacket>) {
    loop {
        match queue.next().await {
            Some(packet) => {
                let result = sink.send(Message::binary(packet.serialize())).await;
                if result.is_err() {
                    eprintln!("client disconnected: aborting.");
                    Connections::remove(&client);
                    return
                }
            },
            None => {
                info!("ending client tunnel");
                return
            },
        };

    }
}