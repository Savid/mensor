//! `mensor run` as a person runs it: the binary, its host and its hand.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

/// The binary, stopped when the test ends however it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// What `GET /v1/minds/main` answers, once it answers.
async fn main_status(client: &reqwest::Client, base: &str) -> Option<Value> {
    let response = client
        .get(format!("{base}/v1/minds/main"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

async fn until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn mensor_run_serves_on_once_its_hand_is_let_go() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    // No model is asked: a key that opens nothing is enough to start.
    let child = Command::new(env!("CARGO_BIN_EXE_mensor"))
        .args(["run", "--listen", &listen, "--dir"])
        .arg(&work)
        .arg("--store")
        .arg(root.path().join("mensor.sqlite"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ANTHROPIC_API_KEY", "not-a-key")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut running = Running(child);
    let (client, base) = (client(), format!("http://{listen}"));

    until("local has joined main", async || {
        main_status(&client, &base)
            .await
            .is_some_and(|status| status["hands"] == json!(["local"]))
    })
    .await;
    let released = client
        .post(format!("{base}/v1/minds/main/release"))
        .json(&json!({"hand": "local"}))
        .send()
        .await
        .unwrap();
    assert_eq!(released.status(), 200);

    sleep(Duration::from_millis(300)).await;
    assert!(running.0.try_wait().unwrap().is_none(), "mensor run exited");
    let status = main_status(&client, &base)
        .await
        .expect("the host stopped serving");
    assert_eq!(status["hands"], json!([]));
    assert!(work.exists(), "the hand's own directory is kept");
}
