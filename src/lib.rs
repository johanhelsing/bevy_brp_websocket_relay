//! WebSocket relay transport for the Bevy Remote Protocol (BRP).
//!
//! This plugin enables BRP on WASM targets by connecting as a WebSocket client
//! to a relay server (e.g. a modified wasm-server-runner). The relay server
//! exposes a standard BRP HTTP endpoint and bridges requests/responses over
//! WebSocket to the Bevy app running in the browser.
//!
//! # Architecture
//!
//! ```text
//! BRP Client -> HTTP :15702 -> Relay Server -> WebSocket -> Browser (this plugin)
//! ```

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_log::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Default WebSocket path for the relay endpoint.
pub const DEFAULT_RELAY_PATH: &str = "brp-relay";

/// Resource that tracks the WebSocket relay connection status.
///
/// Inserted by [`BrpWebSocketRelayPlugin`] and updated automatically
/// when the WebSocket connects or disconnects.
#[derive(Resource, Clone)]
pub struct BrpRelayStatus {
    connected: Arc<AtomicBool>,
}

impl Default for BrpRelayStatus {
    fn default() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl BrpRelayStatus {
    /// Returns `true` if the WebSocket relay is currently connected.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Plugin that connects to a WebSocket relay server and bridges BRP requests
/// into the Bevy app's `BrpSender` channel.
///
/// On non-WASM targets, this plugin is a no-op and logs a warning.
pub struct BrpWebSocketRelayPlugin {
    /// Full WebSocket URL to connect to.
    ///
    /// If `None`, auto-detects from `window.location` using [`Self::path`]
    /// (e.g. `ws://localhost:1334/brp-relay`).
    pub url: Option<String>,
    /// WebSocket path appended when auto-detecting from `window.location`.
    ///
    /// Ignored when [`Self::url`] is set. Defaults to [`DEFAULT_RELAY_PATH`].
    pub path: String,
}

impl Default for BrpWebSocketRelayPlugin {
    fn default() -> Self {
        Self {
            url: None,
            path: DEFAULT_RELAY_PATH.to_string(),
        }
    }
}

impl Plugin for BrpWebSocketRelayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BrpRelayStatus>();

        #[cfg(target_arch = "wasm32")]
        {
            app.insert_resource(wasm::RelayConfig {
                url: self.url.clone(),
                path: self.path.clone(),
            });
            app.add_systems(Startup, wasm::start_websocket_relay);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = app;
            warn!("BrpWebSocketRelayPlugin only works on wasm32 targets, ignoring");
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use bevy_ecs::prelude::*;
    use bevy_log::prelude::*;
    use bevy_remote::{BrpError, BrpMessage, BrpSender};
    use serde_json::Value;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::WebSocket;

    #[derive(Resource)]
    pub(crate) struct RelayConfig {
        pub url: Option<String>,
        pub path: String,
    }

    pub(crate) fn start_websocket_relay(
        brp_sender: Res<BrpSender>,
        config: Res<RelayConfig>,
        status: Res<super::BrpRelayStatus>,
    ) {
        let url = config.url.clone().unwrap_or_else(|| {
            let window = web_sys::window().expect("no global window");
            let location = window.location();
            let host = location.host().expect("no host in location");
            let protocol = if location.protocol().unwrap_or_default() == "https:" {
                "wss:"
            } else {
                "ws:"
            };
            format!("{protocol}//{host}/{}", config.path)
        });

        info!("BRP WebSocket relay: connecting to {url}");

        let ws = WebSocket::new(&url).expect("failed to create WebSocket");

        // Text mode for JSON-RPC messages
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let sender: async_channel::Sender<BrpMessage> = (*brp_sender).clone();
        let ws_for_msg = ws.clone();

        // Handle incoming JSON-RPC requests from the relay
        let onmessage = Closure::<dyn FnMut(_)>::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            let Some(text) = data.dyn_ref::<js_sys::JsString>() else {
                return;
            };
            let text: String = text.into();
            let sender = sender.clone();
            let ws = ws_for_msg.clone();

            wasm_bindgen_futures::spawn_local(async move {
                process_request(text, sender, ws).await;
            });
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let status_connected = status.connected.clone();
        let status_disconnected = status.connected.clone();

        let onopen = Closure::<dyn FnMut()>::new(move || {
            info!("BRP WebSocket relay: connected");
            status_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let onclose = Closure::<dyn FnMut(_)>::new(move |event: web_sys::CloseEvent| {
            warn!(
                "BRP WebSocket relay: disconnected (code={}, reason={})",
                event.code(),
                event.reason()
            );
            status_disconnected.store(false, std::sync::atomic::Ordering::Relaxed);
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        let onerror = Closure::<dyn FnMut(_)>::new(|_: web_sys::ErrorEvent| {
            error!("BRP WebSocket relay: connection error");
        });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();
    }

    /// Process a single JSON-RPC request from the relay.
    async fn process_request(
        text: String,
        sender: async_channel::Sender<BrpMessage>,
        ws: WebSocket,
    ) {
        // Parse JSON-RPC envelope
        let request: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                send_error_response(&ws, None, -32700, &format!("Parse error: {e}"));
                return;
            }
        };

        let id = request.get("id").cloned();
        let method = match request.get("method").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                send_error_response(&ws, id.as_ref(), -32600, "Missing method field");
                return;
            }
        };
        let params = request.get("params").cloned();

        // Create per-request response channel
        let is_watch = method.contains("+watch");
        let channel_size = if is_watch { 8 } else { 1 };
        let (result_sender, result_receiver) = async_channel::bounded(channel_size);

        let message = BrpMessage {
            method,
            params,
            sender: result_sender,
        };

        if sender.send(message).await.is_err() {
            send_error_response(&ws, id.as_ref(), -32603, "BRP channel closed");
            return;
        }

        // For watching requests, stream multiple responses
        if is_watch {
            while let Ok(result) = result_receiver.recv().await {
                let response = make_response(id.as_ref(), result);
                if ws.send_with_str(&response).is_err() {
                    break;
                }
            }
        } else if let Ok(result) = result_receiver.recv().await {
            let response = make_response(id.as_ref(), result);
            let _ = ws.send_with_str(&response);
        }
    }

    fn make_response(id: Option<&Value>, result: Result<Value, BrpError>) -> String {
        let response = match result {
            Ok(value) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            }),
            Err(err) => {
                let error_value =
                    serde_json::to_value(&err).unwrap_or(serde_json::json!({"code": -32603}));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": error_value,
                })
            }
        };
        response.to_string()
    }

    fn send_error_response(ws: &WebSocket, id: Option<&Value>, code: i16, message: &str) {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        });
        let _ = ws.send_with_str(&response.to_string());
    }
}
