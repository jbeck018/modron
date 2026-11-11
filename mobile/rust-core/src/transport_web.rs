//! Web (wasm32) WebSocket transport: the browser's `WebSocket` via
//! web-sys. Browser sockets have no awaitable connect and no read API —
//! events arrive as callbacks, so the constructor bridges
//! `onopen`/`onmessage`/`onclose`/`onerror` into a channel the handshake
//! and the shared run loop drain. The browser answers pings itself; the
//! run loop only ever sees text and close events. The handshake and run
//! loop are driven by `spawn_local` on the JS microtask queue (see
//! `client::spawn_task`).

use crate::client::{handshake_timeout, hello_frame, In};
use crate::error::ClientError;
use modron_protocol::{Frame, Payload, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

/// One established browser connection. The closures must live as long as
/// the socket (dropping them unregisters the callbacks), hence the
/// keep-alive field.
pub(crate) struct Conn {
    ws: web_sys::WebSocket,
    inbound: mpsc::UnboundedReceiver<Result<In, ClientError>>,
    #[allow(dead_code)]
    callbacks: Vec<Closure<dyn FnMut(JsValue)>>,
}

impl Conn {
    /// Send is synchronous on the browser socket; wrapped in async shape
    /// for the shared run loop.
    pub(crate) async fn send_text(&mut self, json: &str) -> Result<(), ClientError> {
        self.ws
            .send_with_str(json)
            .map_err(|_| ClientError::Handshake("socket send failed".into()))
    }

    pub(crate) async fn close(&mut self) {
        // close() throws in CONNECTING/CLOSED states; guard on OPEN.
        if self.ws.ready_state() == web_sys::WebSocket::OPEN {
            let _ = self.ws.close();
        }
    }

    pub(crate) async fn next_in(&mut self) -> Option<Result<In, ClientError>> {
        self.inbound.recv().await
    }
}

fn js_string(v: &JsValue) -> String {
    v.as_string().unwrap_or_else(|| "unknown error".into())
}

/// Open the socket, wait for `onopen` (with a deadline), send the hello
/// frame, and await the welcome. Returns the connection plus the
/// server's reported version.
pub(crate) async fn handshake(
    url: &str,
    token: Option<&str>,
) -> Result<(Conn, String), ClientError> {
    let ws = web_sys::WebSocket::new(url).map_err(|e| ClientError::Connect(js_string(&e)))?;
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let (tx, rx) = mpsc::unbounded_channel();

    // onmessage: text frames only (the server speaks JSON text).
    let onmessage: Closure<dyn FnMut(JsValue)> = {
        let tx = tx.clone();
        Closure::new(move |e: JsValue| {
            let Ok(event) = e.dyn_into::<web_sys::MessageEvent>() else {
                return;
            };
            if let Some(text) = event.data().as_string() {
                let _ = tx.send(Ok(In::Text(text)));
            }
        })
    };
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

    // onerror and onclose both funnel to Closed.
    let onerror: Closure<dyn FnMut(JsValue)> = {
        let tx = tx.clone();
        Closure::new(move |e: JsValue| {
            let _ = tx.send(Err(ClientError::Connect(js_string(&e))));
        })
    };
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

    let onclose: Closure<dyn FnMut(JsValue)> = {
        let tx = tx.clone();
        Closure::new(move |_e: JsValue| {
            let _ = tx.send(Ok(In::Closed));
        })
    };
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

    let onopen: Closure<dyn FnMut(JsValue)> = {
        let tx = tx;
        Closure::new(move |_e: JsValue| {
            let _ = tx.send(Ok(In::Open));
        })
    };
    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

    let mut conn = Conn {
        ws,
        inbound: rx,
        callbacks: vec![onmessage, onerror, onclose, onopen],
    };

    match crate::client::with_timeout(handshake_timeout(), conn.next_in()).await {
        Some(Some(Ok(In::Open))) => {}
        Some(Some(Ok(In::Closed))) | Some(None) => {
            return Err(ClientError::Connect("socket closed before opening".into()))
        }
        Some(Some(Err(e))) => return Err(e),
        Some(Some(Ok(In::Text(_)))) => {
            return Err(ClientError::Handshake("expected open, got a frame".into()))
        }
        None => return Err(ClientError::Timeout),
    }

    let json =
        serde_json::to_string(&hello_frame(token)).map_err(|e| ClientError::Json(e.to_string()))?;
    conn.send_text(&json).await?;

    let first = crate::client::with_timeout(handshake_timeout(), async {
        loop {
            match conn.next_in().await {
                None => return Err(ClientError::Handshake("closed during handshake".into())),
                Some(Ok(In::Text(text))) => return Ok(text),
                Some(Ok(In::Open)) => {}
                Some(Ok(In::Closed)) => {
                    return Err(ClientError::Handshake("closed during handshake".into()))
                }
                Some(Err(e)) => return Err(e),
            }
        }
    })
    .await
    .ok_or(ClientError::Timeout)??;
    let frame =
        serde_json::from_str::<Frame>(&first).map_err(|e| ClientError::Json(e.to_string()))?;
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } if protocol == PROTOCOL_VERSION => Ok((conn, server_version)),
        Payload::Welcome { protocol, .. } => Err(ClientError::ProtocolMismatch {
            server: protocol,
            client: PROTOCOL_VERSION,
        }),
        other => Err(ClientError::Handshake(format!(
            "expected welcome, got {other:?}"
        ))),
    }
}
