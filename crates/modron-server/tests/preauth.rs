//! Pre-auth bounds (security plan B4): an unauthenticated client cannot
//! push a large message and keep the connection open.

mod common;

use common::{spawn_server, WsClient};

#[tokio::test]
async fn unauthenticated_one_mib_first_message_closes_connection() {
    let server = spawn_server(Vec::new(), 0).await;
    let mut client = WsClient::open_ws(server.addr, None)
        .await
        .expect("upgrade succeeds before hello");
    // A 1 MiB text frame before hello: over the 64 KiB pre-auth cap.
    let big = "x".repeat(1024 * 1024);
    client.send_raw_text(&big).await;
    assert!(
        client.expect_closed().await,
        "server must close an unauthenticated oversized connection"
    );
}

#[tokio::test]
async fn small_unauthenticated_frame_still_gets_an_answer() {
    let server = spawn_server(Vec::new(), 0).await;
    let mut client = WsClient::open_ws(server.addr, None).await.unwrap();
    client.send_raw_text("{not json").await;
    let reply = client.recv().await;
    assert!(
        matches!(reply.payload, modron_protocol::Payload::Error { .. }),
        "small pre-auth frames are still parsed and answered"
    );
}
