use rusty_cam::signaling_server::start_server;

#[tokio::main]
async fn main() {
    let server_task = tokio::spawn(async {
        start_server().await;
    });
    let _ = server_task.await;
}
