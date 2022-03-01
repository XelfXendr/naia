use std::time::Duration;

use naia_shared::serde::{BitReader, BitWriter, Serde};
pub use naia_shared::{
    ConnectionConfig, Manifest, PacketType, ProtocolKindType, Protocolize,
    ReplicateSafe, SharedConfig, StandardHeader, Timer, Timestamp, WorldMutType, WorldRefType,
};

use super::io::Io;

#[derive(Debug, PartialEq)]
pub enum HandshakeState {
    AwaitingChallengeResponse,
    AwaitingConnectResponse,
    Connected,
}

pub struct HandshakeManager<P: Protocolize> {
    handshake_timer: Timer,
    pre_connection_timestamp: Timestamp,
    pre_connection_digest: Option<Box<[u8]>>,
    pub connection_state: HandshakeState,
    auth_message: Option<P>,
}

impl<P: Protocolize> HandshakeManager<P> {
    pub fn new(send_interval: Duration) -> Self {
        let mut handshake_timer = Timer::new(send_interval);
        handshake_timer.ring_manual();

        Self {
            handshake_timer,
            pre_connection_timestamp: Timestamp::now(),
            pre_connection_digest: None,
            connection_state: HandshakeState::AwaitingChallengeResponse,
            auth_message: None,
        }
    }

    pub fn set_auth_message(&mut self, auth: P) {
        self.auth_message = Some(auth);
    }

    pub fn is_connected(&self) -> bool {
        return self.connection_state == HandshakeState::Connected;
    }

    // Give handshake manager the opportunity to send out messages to the server
    pub fn send(&mut self, io: &mut Io) {
        if !self.handshake_timer.ringing() {
            return;
        }

        self.handshake_timer.reset();

        match self.connection_state {
            HandshakeState::Connected => {
                // do nothing, not necessary
            }
            HandshakeState::AwaitingChallengeResponse => {
                let mut writer = self.write_challenge_request();
                io.send_writer(&mut writer);
            }
            HandshakeState::AwaitingConnectResponse => {
                let mut writer = self.write_connect_request();
                io.send_writer(&mut writer);
            }
        }
    }

    // Call this regularly so handshake manager can process incoming requests
    pub fn recv(&mut self, reader: &mut BitReader) {
        let header = StandardHeader::de(reader).unwrap();
        match header.packet_type() {
            PacketType::ServerChallengeResponse => {
                self.recv_challenge_response(reader);
            }
            PacketType::ServerConnectResponse => {
                self.recv_connect_response();
            }
            _ => {}
        }
    }

    // Step 1 of Handshake
    pub fn write_challenge_request(&self) -> BitWriter {
        let mut writer = BitWriter::new();
        StandardHeader::new(PacketType::ClientChallengeRequest, 0, 0, 0, 0).ser(&mut writer);

        self.pre_connection_timestamp
            .to_u64()
            .ser(&mut writer);

        writer
    }

    // Step 2 of Handshake
    pub fn recv_challenge_response(&mut self, reader: &mut BitReader) {
        if self.connection_state == HandshakeState::AwaitingChallengeResponse {
            let payload_timestamp = Timestamp::from_u64(&u64::de(reader).unwrap());

            if self.pre_connection_timestamp == payload_timestamp {
                let mut digest_bytes: Vec<u8> = Vec::new();
                for _ in 0..32 {
                    digest_bytes.push(u8::de(reader).unwrap());
                }
                self.pre_connection_digest = Some(digest_bytes.into_boxed_slice());

                self.connection_state = HandshakeState::AwaitingConnectResponse;
            }
        }
    }

    // Step 3 of Handshake
    pub fn write_connect_request(&self) -> BitWriter {
        let mut writer = BitWriter::new();

        StandardHeader::new(PacketType::ClientConnectRequest, 0, 0, 0, 0)
            .ser(&mut writer);

        // write timestamp & digest into payload
        self.write_signed_timestamp(&mut writer);

        // write auth message if there is one
        if let Some(auth_message) = &self.auth_message {
            // write that we have auth
            true.ser(&mut writer);
            // write auth kind
            auth_message.dyn_ref().kind().ser(&mut writer);
            // write payload
            auth_message.write(&mut writer);
        } else {
            // write that we do not have auth
            false.ser(&mut writer);
        }

        writer
    }

    // Step 4 of Handshake
    pub fn recv_connect_response(&mut self) {
        self.connection_state = HandshakeState::Connected;
    }

    // Send 10 disconnect packets
    pub fn write_disconnect(&self) -> BitWriter {
        let mut writer = BitWriter::new();
        StandardHeader::new(PacketType::Disconnect, 0, 0, 0, 0).ser(&mut writer);
        self.write_signed_timestamp(&mut writer);
        writer
    }

    // Private methods

    fn write_signed_timestamp(&self, writer: &mut BitWriter) {
        self.pre_connection_timestamp
            .to_u64()
            .ser(writer);
        for digest_byte in self.pre_connection_digest.as_ref().unwrap().as_ref() {
            digest_byte.ser(writer);
        }
    }
}
