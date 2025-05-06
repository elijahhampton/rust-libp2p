use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use futures::{
    future::FutureExt,  stream::poll_fn,  SinkExt, StreamExt,
};
use libp2p_core::{
    multiaddr::{Multiaddr, Protocol},
    muxing::{StreamMuxerBox, StreamMuxerExt},
    transport::{
        self, Boxed, DialOpts, ListenerId, Transport as _, TransportError, TransportEvent,
    },
    StreamMuxer,
};
use libp2p_identity::{Keypair, PeerId};
use libp2p_noise as noise;
use libp2p_relay::{
    self,
    client::{Transport as CircuitRelayTransport},
    proto::{self, HopMessage},
    protocol, HOP_PROTOCOL_NAME,
};
use libp2p_webrtc_utils::Fingerprint;
use libp2p_webrtc_websys::{Connection, RtcPeerConnection};
use libp2p_websocket_websys::Transport as WebSocketTransport;
use libp2p_yamux as yamux;
use multistream_select::Version;
use wasm_bindgen::JsValue;
use web_sys::RtcConfiguration;

use super::{Signaling, SIGNALING_PROTOCOL_ID};
use crate::{error::Error, pb, signaling::SignalingProtocol};

/// Configuration for WebRTC browser transport.
#[derive(Debug, Clone)]
pub struct Config {
    pub keypair: Keypair,
    pub stun_servers: Vec<String>,
}

/// Config for the [`Transport`].
impl Config {
    pub fn new(keypair: &Keypair) -> Self {
        Self {
            keypair: keypair.clone(),
            stun_servers: vec![],
        }
    }

    pub fn with_stun_server(mut self, server: impl Into<String>) -> Self {
        self.stun_servers.push(server.into());
        self
    }
}

/// A WebRTC [`Transport`] for browser-to-browser connections.
pub struct Transport {
    config: Config,
    pending_events:
        VecDeque<TransportEvent<<Self as libp2p_core::Transport>::ListenerUpgrade, Error>>,
    circuit_relay_transport: CircuitRelayTransport,
    active_relay_connections: HashMap<Multiaddr, Rc<RefCell<StreamMuxerBox>>>,
    reservation_addresses: HashMap<Multiaddr, Vec<Multiaddr>>,
    websocket_transport: Boxed<(PeerId, StreamMuxerBox)>,
}

impl Transport {
    pub fn new(config: Config) -> Self {
        let (circuit_relay_transport, _circuit_relay_client) =
            libp2p_relay::client::new(config.keypair.public().to_peer_id());

        let ws_transport = WebSocketTransport::default()
            .upgrade(Version::V1)
            .authenticate(noise::Config::new(&config.keypair).unwrap())
            .multiplex(yamux::Config::default())
            .boxed();

        Self {
            config,
            pending_events: VecDeque::new(),
            circuit_relay_transport,
            active_relay_connections: HashMap::new(),
            reservation_addresses: HashMap::new(),
            websocket_transport: ws_transport,
        }
    }

    /// Establishes a connection with the relay through a WebSocket and reserves a spot on the
    /// relay.
    pub async fn reserve_relay(&mut self, relay_addr: &Multiaddr) -> Result<(), Error> {
        // Create a websocket connection to the relay
        let ws_conn = self
            .websocket_transport
            .dial(
                relay_addr.clone(),
                DialOpts {
                    role: libp2p_core::Endpoint::Dialer,
                    port_use: libp2p_core::transport::PortUse::New,
                },
            )
            .unwrap()
            .await
            .unwrap();
        let (_peer_id, mut muxer) = ws_conn;

        let substream =
            poll_fn(
                |cx: &mut Context<'_>| match Pin::new(&mut muxer).poll_outbound(cx) {
                    Poll::Ready(Ok(substream)) => Poll::Ready(Some(substream)),
                    Poll::Ready(Err(e)) => Poll::Ready(
                        Err(Error::InvalidMultiaddr(format!(
                            "Error polling outbound: {}",
                            e
                        )))
                        .unwrap(),
                    ),
                    Poll::Pending => Poll::Pending,
                },
            )
            .next()
            .await
            .unwrap();

        tracing::trace!("Attempting to negotiate {} protocol over the stream.", HOP_PROTOCOL_NAME);
        // Negotiate the HOP protocol over the muxer
        let (_protocol, hop_stream) = multistream_select::dialer_select_proto(
            substream,
            &[HOP_PROTOCOL_NAME],
            multistream_select::Version::V1,
        )
        .await
        .unwrap();

        // Frame the HOP stream according to HOP messages
        let mut framed_hop_stream = asynchronous_codec::Framed::new(
            hop_stream,
            quick_protobuf_codec::Codec::<HopMessage>::new(protocol::MAX_MESSAGE_SIZE),
        );

        // Create and send a RESERVE message to the relay
        let reserve_message = proto::HopMessage {
            type_pb: proto::HopMessageType::RESERVE,
            peer: None,
            reservation: None,
            limit: None,
            status: None,
        };

        tracing::trace!("Sending RESERVE HopMessage over stream");
        framed_hop_stream.send(reserve_message).await.unwrap();
        framed_hop_stream.flush().await.unwrap();

        // Wait for the HOP message response and check the status
        let response: HopMessage = framed_hop_stream
            .next()
            .await
            .unwrap()
            .map_err(|e| format!("{}", e.to_string()))
            .unwrap();

        match response.status {
            Some(proto::Status::OK) => {
                tracing::trace!("Received RESERVE message response with Status::OK.");
                let conn_ref = Rc::new(RefCell::new(muxer));

                self.active_relay_connections
                    .insert(relay_addr.clone(), conn_ref.clone());

                if let Some(reservation) = response.reservation {
                    let addrs = reservation
                        .addrs
                        .iter()
                        .filter_map(|bytes| Multiaddr::try_from(bytes.clone()).ok())
                        .collect::<Vec<_>>();

                    self.reservation_addresses.insert(relay_addr.clone(), addrs);
                }

                self.circuit_relay_transport
                    .listen_on(ListenerId::next(), relay_addr.clone())
                    .unwrap();

                return Ok(());
            }
            Some(proto::Status::RESERVATION_REFUSED) => {
                tracing::trace!("Received RESERVE message response with Status::RESERVATION_REFUSED.");
                return Err(Error::InvalidMultiaddr(
                    "Reservation refused by relay".into(),
                ));
            }
            Some(proto::Status::RESOURCE_LIMIT_EXCEEDED) => {
                tracing::trace!("Received RESERVE message response with Status::RESOURCE_LIMIT_EXCEEDED.");
                return Err(Error::InvalidMultiaddr(
                    "Resource limit exceeded on relay".into(),
                ));
            }
            Some(status) => {
                tracing::trace!("Received RESERVE message response with status {:?}.", status);
                return Err(Error::InvalidMultiaddr(format!(
                    "Reservation failed with status: {:?}",
                    status
                )));
            }
            None => {
                tracing::trace!("Received RESERVE message response with missing status");
                return Err(Error::InvalidMultiaddr(
                    "Missing status in relay response".into(),
                ));
            }
        }

        Ok(())
    }
}

impl libp2p_core::Transport for Transport {
    type Output = (PeerId, Connection);
    type Error = Error;
    type ListenerUpgrade =
        futures::future::LocalBoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = futures::future::LocalBoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> std::result::Result<(), TransportError<Self::Error>> {
        if addr.iter().any(|p| p == Protocol::P2pCircuit) {
            self.circuit_relay_transport
                .listen_on(id, addr.clone())
                .unwrap();

            let webrtc_addr = addr.clone().with(Protocol::WebRTC);
            self.pending_events.push_back(TransportEvent::NewAddress {
                listener_id: id,
                listen_addr: webrtc_addr,
            });
            Ok(())
        } else {
            Err(TransportError::MultiaddrNotSupported(addr))
        }
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        let mut found = false;

        self.pending_events.retain(|event| match event {
            TransportEvent::NewAddress { listener_id, .. }
            | TransportEvent::AddressExpired { listener_id, .. }
            | TransportEvent::Incoming { listener_id, .. }
            | TransportEvent::ListenerClosed { listener_id, .. }
            | TransportEvent::ListenerError { listener_id, .. } => {
                if *listener_id == id {
                    found = true;
                    false
                } else {
                    true
                }
            }
        });

        if self.circuit_relay_transport.remove_listener(id) {
            found = true;
        }

        found
    }

    fn dial(
        &mut self,
        addr: Multiaddr,
        dial_opts: libp2p_core::transport::DialOpts,
    ) -> std::result::Result<Self::Dial, TransportError<Self::Error>> {
        tracing::trace!("Attemtping to dial multiaddr {}", addr);

        // Check if the browser WebRTC addr is valid
        if !libp2p_webrtc_utils::is_valid_browser_webrtc_addr(&addr) {
            return Err(TransportError::MultiaddrNotSupported(addr));
        }

        if dial_opts.role.is_listener() {
            return Err(TransportError::MultiaddrNotSupported(addr));
        }

        let (relay_addr, target_peer) = extract_relay_and_target(&addr)
            .ok_or_else(|| TransportError::MultiaddrNotSupported(addr.clone()))?;
        tracing::trace!("Extracted relay address {} and target peer {}.", relay_addr, target_peer);

        let config = self.config.clone();
        let active_relay_conns = self.active_relay_connections.clone();
        let addr = addr.clone();

        // Prepare the RtcConfiguration for the RtcPeerConnection established during the
        // signaling process
        let rtc_config = RtcConfiguration::new();

        if !&config.stun_servers.is_empty() {
            let ice_servers = js_sys::Array::new();

            for server in &config.stun_servers {
                let ice_server = js_sys::Object::new();
                js_sys::Reflect::set(
                    &ice_server,
                    &JsValue::from_str("urls"),
                    &JsValue::from_str(&server),
                )
                .map_err(|err| TransportError::Other(err))
                .unwrap();
                ice_servers.push(&ice_server);
            }

            rtc_config.set_ice_servers(&ice_servers);
        }

        // We need to check if there is an existing relay connection with the relay server. If an
        // existing connection does not exist we reserve a spot on the relay
        let existing_conn = active_relay_conns
            .iter()
            .find(
                |(addr, _rc_conns): &(&Multiaddr, &Rc<RefCell<StreamMuxerBox>>)| {
                    *addr == &relay_addr
                },
            )
            .map(|(_, conn)| conn.clone());

        let relay_connection = {
            match existing_conn {
                Some(conn) => {
                    tracing::trace!("Found existing relay connection");
                    conn
                },
                None => {
                    tracing::trace!("Existing relay connection not found. Reserving relay.");
                    futures::executor::block_on(self.reserve_relay(&relay_addr))
                        .map_err(|e| TransportError::Other(e))?;
                    self.active_relay_connections
                        .get(&relay_addr)
                        .cloned()
                        .ok_or_else(|| {
                            TransportError::Other(Error::InvalidMultiaddr(
                                "Relay connection not found after reservation".into(),
                            ))
                        })
                        .unwrap()
                }
            }
        };

        // Extract the socket address and fingerprint, i.e. certhash.
        let socket_addr = extract_socket_addr(&relay_addr).unwrap();
        let _remote_fingerprint = extract_fingerprint(&addr).unwrap();

        Ok(async move {
            // Now that we have a connection with the target peer over the relay we can create a
            // WebRTC connection and perform the WebRTC signaling process.
            let mut ws_conn = relay_connection.borrow_mut();

            let substream = poll_fn(|cx: &mut Context<'_>| {
                match Pin::new(&mut ws_conn).poll_outbound_unpin(cx) {
                    Poll::Ready(Ok(substream)) => Poll::Ready(Some(substream)),
                    Poll::Ready(Err(e)) => Poll::Ready(
                        Err(Error::InvalidMultiaddr(format!(
                            "Error polling outbound: {}",
                            e
                        )))
                        .unwrap(),
                    ),
                    Poll::Pending => Poll::Pending,
                }
            })
            .next()
            .await
            .unwrap();

            // Create a stream over the existing relay connection for signaling
            let (signaling_protocol, signaling_stream) = multistream_select::dialer_select_proto(
                substream,
                &[SIGNALING_PROTOCOL_ID],
                multistream_select::Version::V1,
            )
            .await
            .unwrap();
            tracing::trace!("Established stream for protocol {}", signaling_protocol);

            let rtc_peer_conn = RtcPeerConnection::new("sha-256".to_string()).await.unwrap();
            let local_fingerprint = rtc_peer_conn.local_fingerprint().unwrap().clone();
            let mut connection = Connection::new(rtc_peer_conn);
            tracing::trace!("Created RTCPeerConnection");

            let signaling_protocol = SignalingProtocol::new();

            tracing::trace!("Attempting to perform signaling process between peers");
            signaling_protocol
                .perform_signaling(&connection.rtc_connection(), signaling_stream, true)
                .await
                .unwrap();
            tracing::trace!("Successfully performed signaling process between peers");

            let remote_fingerprint = extract_fingerprint(&addr).unwrap();
            let noise_stream = connection.new_stream("noise").unwrap();

            let peer_id = libp2p_webrtc_utils::noise::outbound(
                config.keypair,
                noise_stream,
                remote_fingerprint,
                local_fingerprint,
            )
            .await
            .unwrap();

            Ok((peer_id, connection))
        }
        .boxed_local())
    }

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        // Check if there are any existing pending events
        if let Some(pending_event) = self.pending_events.pop_front() {
            return Poll::Ready(pending_event);
        }

        Poll::Pending
    }
}

/// Extracts the relay address and target peer ID from a [`Multiaddr`].
fn extract_relay_and_target(addr: &Multiaddr) -> Option<(Multiaddr, PeerId)> {
    let components: Vec<_> = addr.iter().collect();

    for i in 0..components.len().saturating_sub(1) {
        if components[i] == Protocol::WebRTC && matches!(components[i + 1], Protocol::P2p(_)) {
            // Everything before /webrtc is the relayed multiaddr
            let relay_addr = components[..i]
                .iter()
                .fold(Multiaddr::empty(), |addr, proto| addr.with(proto.clone()));

            if let Protocol::P2p(peer_id) = &components[i + 1] {
                return Some((relay_addr, peer_id.clone()));
            }
        }
    }

    None
}

/// Extracts fingerprint from a [`Multiaddr`].
fn extract_fingerprint(addr: &Multiaddr) -> Result<Fingerprint, Error> {
    for proto in addr.iter() {
        if let Protocol::Certhash(hash) = proto {
            let digest_bytes = hash.digest();
            if digest_bytes.len() != 32 {
                return Err(Error::InvalidMultiaddr(format!(
                    "Invalid fingerprint length: {}",
                    digest_bytes.len()
                )));
            }
            let mut array = [0u8; 32];
            array.copy_from_slice(&digest_bytes);
            return Ok(Fingerprint::raw(array));
        }
    }

    // If we can't find a certhash in the multiaddress we result to a default
    // fingerprint
    Ok(Fingerprint::raw([0u8; 32]))
}

/// Extracts the socket address from a [`Multiaddr`].
fn extract_socket_addr(addr: &Multiaddr) -> Result<SocketAddr, Error> {
    let mut ip = None;
    let mut port = None;

    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(ip_addr) => ip = Some(std::net::IpAddr::V4(ip_addr)),
            Protocol::Ip6(ip_addr) => ip = Some(std::net::IpAddr::V6(ip_addr)),
            Protocol::Tcp(p) | Protocol::Udp(p) => port = Some(p),
            _ => {}
        }
    }

    if let (Some(ip_addr), Some(port_num)) = (ip, port) {
        Ok(SocketAddr::new(ip_addr, port_num))
    } else {
        Err(Error::InvalidMultiaddr("Missing IP address or port".into()))
    }
}
