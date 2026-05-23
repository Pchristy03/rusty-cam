use std::{
    collections::HashMap,
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

use crate::{camera_peer::connect_camera_to_ws, utils::SignalMessage};

type Tx = mpsc::UnboundedSender<Message>;
type Peers = Arc<Mutex<HashMap<String, Tx>>>;

pub async fn start_server() {
    async fn handler(ws: WebSocketUpgrade, peers: Peers) -> Response {
        ws.on_upgrade(move |socket| handle_socket(socket, peers.clone()))
    }

    async fn handle_socket(socket: WebSocket, peers: Peers) {
        println!(
            "[WS] new connection (peers in map: {})",
            peers.lock().unwrap().len()
        );
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
                            continue;
                        }
                    };

                    // Viewers never send Register; tie this socket to their id on first Offer so we
                    // remove them from the peers map when the tab disconnects or refreshes.
                    if let SignalMessage::Offer { from, .. } = &incoming {
                        current_peer = Some(from.clone());
                    }

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
                                println!("Sending Offer to: {}", offer_to);
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
                                println!("Sending Answer to: {}", answer_to);
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
                                println!("Sending Candidate to: {}", to);
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
                                println!("Sending Video to: {}", to);
                            }
                        } else {
                            println!("Peer with Id: {} not present", to)
                        }
                    }
                }
                Message::Ping(payload) => {
                    if sendertx.send(Message::Pong(payload)).is_err() {
                        break;
                    }
                }
                Message::Pong(data) => {
                    println!("Recieved Pong... {:?}", data)
                }
                Message::Close(_) => {
                    println!("WebSocket closed gracefully");
                    break;
                }
                Message::Binary(_) => {
                    eprintln!("Ignoring unexpected WebSocket binary frame");
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

    const CAM_HTML_TEMPLATE: &str = include_str!("static/cam.html");

    async fn home_page() -> Html<String> {
        // tokio::spawn(connect_camera_to_ws());
        let version = concat!("rusty-cam v", env!("CARGO_PKG_VERSION"));
        Html(CAM_HTML_TEMPLATE.replace("__RUSTY_CAM_VERSION__", version))
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
            let mut locked = peers_clone.lock().unwrap();
            locked.retain(|key, sender| {
                // One text frame per tick: enough for the viewer to refresh `lastPing`, and half
                // the channel traffic vs Ping+Text (matters on a Pi with journald + many tabs).
                if sender
                    .send(Message::Text("{ \"t\": \"ping\"}".into()))
                    .is_err()
                {
                    println!("Removing dead peer: {}", key);
                    false
                } else {
                    true
                }
            });
            println!("-- CONNECTED PEERS ({}) --", locked.len());
            for (key, _) in locked.iter() {
                println!("  {}", key);
            }
        }
    });

    // Bind the server
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server running on {}", addr);

    // This future never returns unless the server errors
    let listener = TcpListener::bind(addr).await.unwrap();
    // One capture pipeline + signaling client for the whole process (not per browser page).
    tokio::spawn(connect_camera_to_ws());
    axum::serve(listener, app).await.unwrap();
}
