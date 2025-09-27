use std::{thread::sleep, time::Duration};

use rusty_cam::{camera_peer::connect_camera_to_ws, signaling_server::start_server};

#[tokio::main]
async fn main() {
    let server_task = tokio::spawn(async {
        start_server().await;
    });

    sleep(Duration::from_millis(2000));

    connect_camera_to_ws().await;

    let _ = server_task.await;
}
