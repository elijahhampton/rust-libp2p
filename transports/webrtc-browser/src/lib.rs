mod error;
mod proto_stream;
mod signaling;
mod transport;

pub(crate) mod pb {
    include!(concat!(env!("OUT_DIR"), "/signaling.rs"));
}

pub use self::{
    proto_stream::ProtobufStream,
    signaling::{Signaling, SignalingProtocol, SIGNALING_PROTOCOL_ID},
    transport::{Config, Transport},
};
