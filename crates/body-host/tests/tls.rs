//! A body can dial a `wss://` address.

use std::time::Duration;

use mensor_body::{Capabilities, Core, Limits};
use mensor_body_host::{Backoff, Config, WebSocket, boot, start};
use mensor_protocol::Name;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_tls_dial_that_fails_is_tried_again() {
    // A listener that speaks no TLS: every handshake fails.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepting = tokio::spawn(async move {
        let mut dials = 0;
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
            dials += 1;
            if dials == 3 {
                return;
            }
        }
    });

    let core = Core::new(Name::new("a1").unwrap(), boot(), Limits::default());
    let dial = WebSocket::new(format!("wss://{address}/v1/attach"));
    let mut config = Config::new(dial, Capabilities::new());
    config.backoff = Backoff {
        min: Duration::from_millis(5),
        max: Duration::from_millis(20),
    };
    let (body, task) = start(core, config);

    tokio::time::timeout(Duration::from_secs(10), accepting)
        .await
        .expect("the body dialled three times")
        .unwrap();
    assert!(
        !task.is_finished(),
        "a failed handshake is worth another dial"
    );
    body.stop();
    task.await.unwrap().unwrap();
}
