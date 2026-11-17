//! The binaries as a person runs them: `mensor run`, the host alone and
//! `mensor token`.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Running, spawn};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// A free port on loopback, and its address.
fn listen() -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("127.0.0.1:{port}")
}

async fn until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(50)).await;
    }
}

/// What `GET url` answers, with `token` as the bearer token if there is
/// one, if it answers with success.
async fn get(client: &reqwest::Client, url: &str, token: Option<&str>) -> Option<Value> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

#[tokio::test]
async fn mensor_run_serves_on_once_its_hand_is_let_go() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let listen = listen();
    let store = root.path().join("mensor.sqlite");
    // No model is asked: a key that opens nothing is enough to start.
    let mut running = spawn(
        env!("CARGO_BIN_EXE_mensor"),
        &[
            "run",
            "--listen",
            &listen,
            "--dir",
            work.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
        ],
        &[("ANTHROPIC_API_KEY", "not-a-key")],
    );
    let (client, main) = (client(), format!("http://{listen}/v1/minds/main"));

    until("local has joined main", async || {
        get(&client, &main, None)
            .await
            .is_some_and(|status| status["hands"] == json!(["local"]))
    })
    .await;
    let released = client
        .post(format!("{main}/release"))
        .json(&json!({"hand": "local"}))
        .send()
        .await
        .unwrap();
    assert_eq!(released.status(), 200);

    sleep(Duration::from_millis(300)).await;
    assert!(running.0.try_wait().unwrap().is_none(), "mensor run exited");
    let status = get(&client, &main, None)
        .await
        .expect("the host stopped serving");
    assert_eq!(status["hands"], json!([]));
    assert!(work.exists(), "the hand's own directory is kept");
}

/// A Messages API whose model asks for `command` once, then ends its turn.
#[cfg(target_os = "linux")]
async fn model(command: &'static str) -> String {
    let answer = move |axum::Json(body): axum::Json<Value>| async move {
        let usage = json!({"input_tokens": 1, "output_tokens": 1});
        axum::Json(if body["messages"].to_string().contains("tool_result") {
            json!({"content": [{"type": "text", "text": "Done."}], "stop_reason": "end_turn", "usage": usage})
        } else {
            let asked = json!({"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": command}});
            json!({"content": [asked], "stop_reason": "tool_use", "usage": usage})
        })
    };
    let app = axum::Router::new().route("/v1/messages", axum::routing::post(answer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{address}")
}

/// `mensor run` whose model asks for `command` once, with fake credentials:
/// the process, and the history of its mind once the model has its answer.
#[cfg(target_os = "linux")]
async fn run_once(command: &'static str, work: &std::path::Path) -> (Running, String) {
    let base = model(command).await;
    let listen = listen();
    let running = spawn(
        env!("CARGO_BIN_EXE_mensor"),
        &["run", "--listen", &listen, "--dir", work.to_str().unwrap()],
        &[
            ("ANTHROPIC_API_KEY", "fake-model-key"),
            ("ANTHROPIC_BASE_URL", &base),
            ("MENSOR_TOKEN", "fake-host-token"),
        ],
    );
    let (client, main) = (client(), format!("http://{listen}/v1/minds/main"));
    let token = Some("fake-host-token");
    until("local has joined main", async || {
        get(&client, &main, token)
            .await
            .is_some_and(|status| status["hands"] == json!(["local"]))
    })
    .await;

    let prompted = client
        .post(format!("{main}/prompt"))
        .bearer_auth("fake-host-token")
        .json(&json!({"text": "run it"}))
        .send()
        .await
        .unwrap();
    assert_eq!(prompted.status(), 202);
    let mut history = String::new();
    until("the model has its answer", async || {
        let read = get(&client, &format!("{main}/history"), token).await;
        history = read.map(|read| read.to_string()).unwrap_or_default();
        history.contains("Done.")
    })
    .await;
    (running, history)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_command_cannot_read_the_credentials_of_the_mensor_run_it_runs_in() {
    if common::skipped_as_root("a_command_cannot_read_the_credentials_of_the_mensor_run") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let (running, history) = run_once(common::PROBE, work.path()).await;

    // The command looked at the process it runs in, and was refused.
    let parent = format!("parent={}", running.0.id());
    assert!(history.contains(&parent), "{history}");
    assert!(history.contains("Permission denied"), "{history}");
    assert!(!history.contains("fake-model-key"), "{history}");
    assert!(!history.contains("fake-host-token"), "{history}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn what_a_command_leaves_running_ends_with_the_mensor_run_a_signal_stops() {
    let work = tempfile::tempdir().unwrap();
    let (mut running, history) = run_once(common::LEAVES, work.path()).await;
    let leftover = common::leftover(&history);
    assert!(!common::gone(leftover), "{leftover} ended with its call");

    assert!(running.terminate().success());
    common::ends(leftover);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_host_alone_hides_its_environment_from_its_users_other_processes() {
    if common::skipped_as_root("the_host_alone_hides_its_environment") {
        return;
    }
    let listen = listen();
    let running = spawn(
        env!("CARGO_BIN_EXE_mensor-mind"),
        &["--listen", &listen],
        &[
            ("ANTHROPIC_API_KEY", "fake-model-key"),
            ("MENSOR_TOKEN", "fake-host-token"),
        ],
    );
    let (client, minds) = (client(), format!("http://{listen}/v1/minds"));
    until("the host serves", async || {
        get(&client, &minds, Some("fake-host-token"))
            .await
            .is_some()
    })
    .await;

    let pid = running.0.id();
    for file in ["environ", "mem"] {
        let read = std::fs::read(format!("/proc/{pid}/{file}"));
        let error = read.expect_err(file);
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{file}");
    }
}

#[test]
fn mensor_token_prints_a_token_for_one_mind_made_from_the_hosts() {
    let token = |mind: &str, vars: &[(&str, &str)]| {
        Command::new(env!("CARGO_BIN_EXE_mensor"))
            .args(["token", "--mind", mind])
            .env_clear()
            .envs(vars.iter().copied())
            .output()
            .unwrap()
    };
    let host = [("MENSOR_TOKEN", "fake-host-token")];

    let made = token("m", &host);
    assert!(made.status.success());
    let expected = mensor_mind_host::mind_token(
        &mensor_mind_host::Token::new("fake-host-token").unwrap(),
        &mensor_protocol::Name::new("m").unwrap(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(made.stdout).unwrap(),
        format!("{expected}\n")
    );

    for without in [token("m", &[]), token("m", &[("MENSOR_TOKEN", "")])] {
        assert!(!without.status.success());
        let told = String::from_utf8(without.stderr).unwrap();
        assert!(told.contains("MENSOR_TOKEN holds no host token"), "{told}");
    }

    let child = token("m.d3342iuj", &host);
    assert!(!child.status.success());
    assert!(child.stdout.is_empty());
}

/// What `mensor serve` says as it refuses to start, given only `vars` and
/// `args`: its error, and its exit.
fn refused(args: &[&str], vars: &[(&str, &str)]) -> String {
    let ran = Command::new(env!("CARGO_BIN_EXE_mensor"))
        .arg("serve")
        .args(args)
        .env_clear()
        .envs(vars.iter().copied())
        .output()
        .unwrap();
    assert!(!ran.status.success(), "it started");
    String::from_utf8(ran.stderr).unwrap()
}

/// A Postgres URL nothing answers at, with a password it must never show.
const UNREACHED: &str = "postgres://mensor:not-a-secret@127.0.0.1:1/m?sslmode=disable";

#[test]
fn a_host_keeps_its_histories_in_one_store_and_is_refused_two() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mensor.sqlite");
    let said = refused(
        &["--store", file.to_str().unwrap()],
        &[("MENSOR_POSTGRES", UNREACHED)],
    );
    assert!(
        said.contains("--store and MENSOR_POSTGRES are both set"),
        "{said}"
    );
    assert!(!said.contains("not-a-secret"), "{said}");
    assert!(!file.exists(), "it made the file");
}

#[cfg(not(feature = "postgres"))]
#[test]
fn a_mensor_built_without_postgres_refuses_a_postgres_url() {
    let said = refused(&[], &[("MENSOR_POSTGRES", UNREACHED)]);
    assert!(said.contains("built without Postgres"), "{said}");
    assert!(!said.contains("not-a-secret"), "{said}");
}

#[cfg(feature = "postgres")]
#[test]
fn a_host_that_cannot_reach_its_postgres_says_so_without_its_password() {
    let said = refused(&[], &[("MENSOR_POSTGRES", UNREACHED)]);
    assert!(said.contains("error connecting to server"), "{said}");
    assert!(!said.contains("not-a-secret"), "{said}");
}
