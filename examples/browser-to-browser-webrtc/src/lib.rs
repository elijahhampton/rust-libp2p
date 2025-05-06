use std::str::FromStr;

use js_sys::{Object, Reflect};
use libp2p::core::transport::DialOpts;
use libp2p_core::{multiaddr::Multiaddr, transport::Transport as _};
use libp2p_identity::Keypair;
use libp2p_webrtc_browser::{Config, Transport};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn initialize() {
    #[cfg(feature = "console_log")]
    console_log::init_with_level(log::Level::Info).unwrap();

    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_console_config(tracing_wasm::ConsoleConfig::ReportWithConsoleColor)
            .build(),
    );
}

#[wasm_bindgen]
pub struct BrowserTransport {
    inner: Transport,
    keypair: Keypair,
}

#[wasm_bindgen]
impl BrowserTransport {
    #[wasm_bindgen(constructor)]
    pub fn new(stun_server: &str) -> Result<BrowserTransport, JsValue> {
        let keypair = Keypair::generate_ed25519();
        let config = Config::new(&keypair).with_stun_server(stun_server);
        Ok(BrowserTransport {
            inner: Transport::new(config),
            keypair,
        })
    }

    pub fn peer_id(&self) -> String {
        self.keypair.public().to_peer_id().to_string()
    }

    pub async fn listen(&mut self, relay_addr: &str) -> Result<(), JsValue> {
        let addr = Multiaddr::from_str(relay_addr)
            .map_err(|e| JsValue::from_str(&format!("Invalid relay addr: {}", e)))?;

        self.inner
            .reserve_relay(&addr)
            .await
            .map_err(|e| JsValue::from_str(&format!("Relay reservation failed: {}", e)))?;

        Ok(())
    }

    pub async fn dial(&mut self, full_addr: &str) -> Result<JsValue, JsValue> {
        let addr = Multiaddr::from_str(full_addr)
            .map_err(|e| JsValue::from_str(&format!("Invalid addr: {}", e)))?;

        let dial_future = self
            .inner
            .dial(
                addr,
                DialOpts {
                    role: libp2p::core::Endpoint::Dialer,
                    port_use: libp2p::core::transport::PortUse::New,
                },
            )
            .map_err(|e| JsValue::from_str(&format!("Dial failed: {}", e)))?;

        let (peer_id, connection) = dial_future
            .await
            .map_err(|e| JsValue::from_str(&format!("Dial failed: {}", e)))?;

        let data_channel = connection.rtc_connection().create_data_channel("chat");

        let result = Object::new();
        Reflect::set(
            &result,
            &JsValue::from_str("peerId"),
            &JsValue::from_str(&peer_id.to_string()),
        )
        .map_err(|e| {
            JsValue::from_str(&format!("Set peerId failed: {}", e.as_string().unwrap()))
        })?;
        Reflect::set(
            &result,
            &JsValue::from_str("dataChannel"),
            &data_channel.into(),
        )
        .map_err(|e| {
            JsValue::from_str(&format!(
                "Set dataChannel failed: {}",
                e.as_string().unwrap()
            ))
        })?;

        Ok(result.into())
    }
}

#[wasm_bindgen]
pub fn generate_peer_id() -> String {
    let keypair = Keypair::generate_ed25519();
    keypair.public().to_peer_id().to_string()
}
