use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{Html, Response},
    routing::{any, get},
};

use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, sync::mpsc};

use crate::utils::SignalMessage;

use bytes::Bytes;

type Tx = mpsc::UnboundedSender<Message>;
type Peers = Arc<Mutex<HashMap<String, Tx>>>;

pub async fn start_server() {
    async fn handler(ws: WebSocketUpgrade, peers: Peers) -> Response {
        ws.on_upgrade(move |socket| handle_socket(socket, peers.clone()))
    }

    async fn handle_socket(socket: WebSocket, peers: Peers) {
        let mut current_peer: Option<String> = None;
        let (mut write, mut read) = socket.split();
        let (sendertx, mut receiverrx) = mpsc::unbounded_channel::<Message>();

        let send_task = tokio::spawn(async move {
            while let Some(msg) = receiverrx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Receive messages from the socket
        while let Some(Ok(msg)) = read.next().await {
            match msg {
                Message::Text(text) => {
                    let incoming: SignalMessage = match serde_json::from_str(&text) {
                        Ok(msg) => msg,
                        Err(e) => {
                            eprintln!("Invalid signaling message: {e}");
                            return;
                        }
                    };

                    if current_peer.is_none() {
                        if let SignalMessage::Register { from } = incoming.clone() {
                            println!("Adding Id: {} via register", from);
                            peers
                                .lock()
                                .unwrap()
                                .insert(from.to_string(), sendertx.clone());
                            current_peer = Some(from);
                        }
                    }

                    // offers
                    if let SignalMessage::Offer { offer_to, from, .. } = incoming.clone() {
                        peers
                            .lock()
                            .unwrap()
                            .insert(from.to_string(), sendertx.clone());
                        if let Some(tx) = peers.lock().unwrap().get(&offer_to) {
                            if tx.send(Message::Text(text.clone())).is_err() {
                                eprintln!("failed to send to peer {} (receiver dropped)", offer_to);
                            } else {
                                println!("Sending to: {}", offer_to);
                            }
                        } else {
                            println!("Peer with Id: {} not present", offer_to)
                        }
                    }

                    // answers
                    if let SignalMessage::Answer { answer_to, .. } = incoming.clone() {
                        if let Some(tx) = peers.lock().unwrap().get(&answer_to) {
                            if tx.send(Message::Text(text.clone())).is_err() {
                                eprintln!(
                                    "failed to send to peer {} (receiver dropped)",
                                    answer_to
                                );
                            } else {
                                println!("Sending to: {}", answer_to);
                            }
                        } else {
                            println!("Peer with Id: {} not present", answer_to)
                        }
                    }

                    //candidates
                    if let SignalMessage::Candidate { to, .. } = incoming.clone() {
                        if let Some(tx) = peers.lock().unwrap().get(&to) {
                            if tx.send(Message::Text(text.clone())).is_err() {
                                eprintln!("failed to send to peer {} (receiver dropped)", to);
                            } else {
                                println!("Sending to: {}", to);
                            }
                        } else {
                            println!("Peer with Id: {} not present", to)
                        }
                    }

                    // Video Request
                    if let SignalMessage::Video { to, .. } = incoming {
                        if let Some(tx) = peers.lock().unwrap().get(&to) {
                            if tx.send(Message::Text(text)).is_err() {
                                eprintln!("failed to send to peer {} (receiver dropped)", to);
                            } else {
                                println!("Sending to: {}", to);
                            }
                        } else {
                            println!("Peer with Id: {} not present", to)
                        }
                    }
                }
                Message::Pong(data) => {
                    println!("Recieved Pong... {:?}", data)
                }
                Message::Close(_) => {
                    println!("WebSocket closed gracefully");
                    break;
                }
                _ => {
                    println!("Unsupported Message Type. Exiting peer connection...");
                    break;
                }
            }
        }

        // Cleanup when disconnected
        if let Some(id) = current_peer.take() {
            let mut locked = peers.lock().unwrap();
            locked.remove(&id);
            println!("Removed peer id {} from peers map", id);
        }
        let _ = send_task.await;
    }

    let peers: Peers = Arc::new(Mutex::new(HashMap::new()));

    async fn home_page() -> Html<String> {
        let html = include_str!("static/cam.html");
        Html(html.to_string())
    }

    // Build the router
    let app = Router::new()
        .route(
            "/ws",
            any({
                let peers = peers.clone();
                move |ws| handler(ws, peers)
            }),
        )
        .route("/home", get(home_page));

    const PING_INTERVAL: u64 = 5;
    let peers_clone = peers.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL));

        loop {
            interval.tick().await;
            peers_clone.lock().unwrap().retain(|key, sender| {
                if sender
                    .send(Message::Ping(Bytes::from(format!("{}", key))))
                    .is_err()
                    || sender
                        .send(Message::Text("{ \"t\": \"ping\"}".into()))
                        .is_err()
                {
                    println!("Removing dead peer: {}", key);
                    false
                } else {
                    true
                }
            });
            println!("-- CONNECTED PEERS --\n");
            for (key, _) in peers.lock().unwrap().iter() {
                println!("{}", key)
            }
            println!("\n---------------------");
        }
    });

    // Bind the server
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server running on {}", addr);

    // This future never returns unless the server errors
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
