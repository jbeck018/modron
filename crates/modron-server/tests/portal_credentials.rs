//! Portal credentials (PLAN 10.9, item 2) against a fake bridge: the fetch,
//! its bearer, the overlay replacement rules, the refusals, and the start-up
//! flag rule. Each test owns its overlay; nothing here touches the process one.

use modron_core::credential_overlay::CredentialOverlay;
use modron_server::rc::{
    check_portal_credentials, portal_credentials_on, refresh_portal_credentials,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Serves `responses` in order, one per connection, and records each request
/// head (lower-cased). Returns the ws:// bridge base and the recorded heads.
fn fake_bridge(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heads);
    std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase());
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (format!("ws://{addr}"), heads)
}

fn keys(body: &str) -> String {
    body.to_string()
}

#[tokio::test]
async fn fetch_fills_the_overlay_and_sends_the_bearer() {
    let (bridge, heads) = fake_bridge(vec![(
        200,
        keys(r#"{"credentials":{"ZAI_API_KEY":"z-vault","lower_case":"no","EMPTY":""}}"#),
    )]);
    let overlay = CredentialOverlay::new();
    let names = refresh_portal_credentials(&bridge, "host-tok-1", &overlay)
        .await
        .unwrap();
    assert_eq!(
        names,
        vec!["ZAI_API_KEY"],
        "invalid and empty names are dropped"
    );
    assert_eq!(overlay.get("ZAI_API_KEY").as_deref(), Some("z-vault"));
    let head = heads.lock().unwrap()[0].clone();
    assert!(head.starts_with("get /api/host/credentials"), "{head}");
    assert!(head.contains("authorization: bearer host-tok-1"), "{head}");
}

#[tokio::test]
async fn a_removed_vault_key_disappears_after_the_next_fetch() {
    let (bridge, _heads) = fake_bridge(vec![
        (
            200,
            keys(r#"{"credentials":{"ZAI_API_KEY":"z","GROQ_API_KEY":"g"}}"#),
        ),
        (200, keys(r#"{"credentials":{"GROQ_API_KEY":"g"}}"#)),
    ]);
    let overlay = CredentialOverlay::new();
    refresh_portal_credentials(&bridge, "t", &overlay)
        .await
        .unwrap();
    assert!(overlay.get("ZAI_API_KEY").is_some());
    refresh_portal_credentials(&bridge, "t", &overlay)
        .await
        .unwrap();
    assert_eq!(
        overlay.get("ZAI_API_KEY"),
        None,
        "removed in the vault, gone here"
    );
    assert_eq!(overlay.get("GROQ_API_KEY").as_deref(), Some("g"));
}

#[tokio::test]
async fn a_failed_fetch_keeps_the_previous_set() {
    let (bridge, _heads) = fake_bridge(vec![
        (200, keys(r#"{"credentials":{"ZAI_API_KEY":"z"}}"#)),
        (503, "host credentials are disabled".to_string()),
    ]);
    let overlay = CredentialOverlay::new();
    refresh_portal_credentials(&bridge, "t", &overlay)
        .await
        .unwrap();
    let err = refresh_portal_credentials(&bridge, "t", &overlay).await;
    assert!(err.is_err(), "a 503 is an error");
    assert_eq!(
        overlay.get("ZAI_API_KEY").as_deref(),
        Some("z"),
        "previous set kept"
    );
}

#[tokio::test]
async fn a_refused_bearer_leaves_the_overlay_empty() {
    let (bridge, _heads) = fake_bridge(vec![(401, "unauthorized".to_string())]);
    let overlay = CredentialOverlay::new();
    let err = refresh_portal_credentials(&bridge, "wrong", &overlay).await;
    let text = format!("{:#}", err.unwrap_err());
    assert!(text.contains("401"), "{text}");
    assert!(
        !text.contains("wrong"),
        "the token is never in an error: {text}"
    );
    assert!(overlay.names().is_empty());
}

#[tokio::test]
async fn a_non_json_answer_is_an_error_not_a_crash() {
    let (bridge, _heads) = fake_bridge(vec![(200, "<html>not json</html>".to_string())]);
    let overlay = CredentialOverlay::new();
    assert!(refresh_portal_credentials(&bridge, "t", &overlay)
        .await
        .is_err());
    assert!(overlay.names().is_empty());
}

#[tokio::test]
async fn plain_http_off_loopback_is_refused_without_a_request() {
    let overlay = CredentialOverlay::new();
    let err = refresh_portal_credentials("ws://bridge.example.com", "t", &overlay).await;
    let text = format!("{:#}", err.unwrap_err());
    assert!(text.contains("wss://"), "names the secure scheme: {text}");
    let https = refresh_portal_credentials("https://bridge.example.com", "t", &overlay).await;
    assert!(
        https.is_err(),
        "an https base is refused: the tunnel rule takes ws/wss"
    );
    assert!(overlay.names().is_empty());
}

#[test]
fn the_flag_needs_remote_control_and_the_host_token() {
    assert!(check_portal_credentials(false, None, None).is_ok());
    assert!(check_portal_credentials(true, Some("wss://b.example"), Some("t")).is_ok());
    let no_remote = check_portal_credentials(true, None, Some("t")).unwrap_err();
    assert!(format!("{no_remote}").contains("--remote-control"));
    let no_token = check_portal_credentials(true, Some("wss://b.example"), None).unwrap_err();
    assert!(format!("{no_token}").contains("MODRON_HOST_TOKEN"));
    assert!(check_portal_credentials(true, Some("wss://b.example"), Some("")).is_err());
}

#[test]
fn the_env_switch_reads_one_or_true() {
    let on = |v: &'static str| {
        move |name: &str| (name == "MODRON_PORTAL_CREDENTIALS").then(|| v.to_string())
    };
    assert!(portal_credentials_on(&on("1")));
    assert!(portal_credentials_on(&on("true")));
    assert!(!portal_credentials_on(&on("0")));
    assert!(!portal_credentials_on(&on("")));
    assert!(!portal_credentials_on(&|_: &str| None));
}
