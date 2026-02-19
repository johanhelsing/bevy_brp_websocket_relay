# bevy_brp_websocket_relay

WebSocket relay transport for the [Bevy Remote Protocol (BRP)](https://docs.rs/bevy_remote) — enables BRP on WASM targets.

> **warning**: This crate was largely written by a coding agent. It works, but use at your own risk.

## Problem

BRP normally uses an HTTP server inside the Bevy app to accept JSON-RPC requests. Browsers can't bind HTTP server sockets, so BRP doesn't work on WASM out of the box.

## Solution

This plugin flips the connection direction. Instead of the Bevy app listening for HTTP, it connects *outbound* as a WebSocket client to a relay server. The relay server accepts standard BRP HTTP requests and bridges them over WebSocket to the browser.

```
BRP Client (editor, CLI tool, etc.)
        |
   HTTP POST :15702
        |
   Relay Server
        |
   WebSocket /brp-relay
        |
   Browser Bevy App (this plugin)
```

Any tool that speaks BRP over HTTP works transparently — the relay is invisible to the client.

## Usage

Add the plugin alongside `RemotePlugin`:

```rust
use bevy::prelude::*;
use bevy::remote::RemotePlugin;
use bevy_brp_websocket_relay::BrpWebSocketRelayPlugin;

App::new()
    .add_plugins(DefaultPlugins)
    .add_plugins(RemotePlugin::default())
    .add_plugins(BrpWebSocketRelayPlugin::default())
    .run();
```

On non-WASM targets, the plugin is a no-op (logs a warning and does nothing).

### URL auto-detection

By default, the plugin derives the WebSocket URL from `window.location` and the configured `path`:

- `http://localhost:1334` -> `ws://localhost:1334/brp-relay`
- `https://example.com` -> `wss://example.com/brp-relay`

You can change the path (e.g. for a standalone relay on a different endpoint):

```rust
BrpWebSocketRelayPlugin {
    path: "my-relay".to_string(),
    ..default()
}
```

Or provide a full URL to bypass auto-detection entirely:

```rust
BrpWebSocketRelayPlugin {
    url: Some("ws://localhost:9000/brp".to_string()),
    ..default()
}
```

## Relay Server

The other half of this system is a relay server that:

1. Accepts WebSocket connections from the browser at `/brp-relay`
2. Accepts BRP HTTP POST requests (default port 15702)
3. Bridges JSON-RPC requests/responses between HTTP and WebSocket

The relay protocol is simple enough that it can be embedded into an existing dev server or run as a standalone process. Two options:

### Embedded in wasm-server-runner

A fork of [wasm-server-runner](https://github.com/jakobhellermann/wasm-server-runner) with built-in relay support is available at [johanhelsing/wasm-server-runner](https://github.com/johanhelsing/wasm-server-runner) (branch `brp-relay`). This is the easiest option for development — it serves the WASM app and the relay in one process.

Configuration via environment variable `WASM_SERVER_RUNNER_BRP_PORT`:

- Default: `15702` (standard BRP port)
- Set to `0`, `false`, `off`, or `no` to disable the relay

### Standalone relay

For production deployments or custom setups, the relay can also run as a standalone server. The protocol is straightforward:

- **WebSocket** (`/brp-relay`): The browser connects here. Messages are JSON-RPC request/response strings.
- **HTTP POST** (port 15702): BRP tools send JSON-RPC requests here. The relay forwards them over WebSocket and returns the response.

Requests are matched to responses by JSON-RPC `id`. See the [wasm-server-runner implementation](https://github.com/johanhelsing/wasm-server-runner/blob/brp-relay/src/server/brp_relay.rs) for a reference (~170 lines of Rust/axum).

## How it works

1. On `Startup`, the plugin opens a WebSocket to the relay server
2. Incoming WebSocket messages are parsed as JSON-RPC requests
3. Requests are forwarded into Bevy's `BrpSender` channel (the same channel `RemotePlugin` uses)
4. Responses are sent back over the WebSocket to the relay
5. The relay returns the response to the original HTTP caller

Watch requests (`+watch` methods) are supported — multiple responses stream back over the same WebSocket message flow.

## Compatibility

| bevy | bevy_brp_websocket_relay |
|------|--------------------------|
| 0.18 | 0.1                      |
