use std::{hash::Hash, net::SocketAddr};

use naia_serde::{BitWriter, Serde};
use naia_socket_shared::Instant;

use crate::{
    backends::Timer,
    messages::{channels::channel_kinds::ChannelKinds, message_manager::MessageManager},
    types::{HostType, PacketIndex},
    world::{
        entity::entity_converters::{EntityConverterMut, GlobalWorldManagerType},
        host::{host_world_manager::HostWorldEvents, host_world_writer::HostWorldWriter},
        local_world_manager::LocalWorldManager,
        remote::remote_world_reader::RemoteWorldReader,
    },
    EntityEvent, HostWorldManager, Protocol, RemoteWorldManager, WorldMutType, WorldRefType,
};

use super::{
    ack_manager::AckManager, connection_config::ConnectionConfig,
    packet_notifiable::PacketNotifiable, packet_type::PacketType, standard_header::StandardHeader,
};

/// Represents a connection to a remote host, and provides functionality to
/// manage the connection and the communications to it
pub struct BaseConnection<E: Copy + Eq + Hash + Send + Sync> {
    pub message_manager: MessageManager,
    pub host_world_manager: HostWorldManager<E>,
    pub remote_world_manager: RemoteWorldManager<E>,
    pub remote_world_reader: RemoteWorldReader<E>,
    pub local_world_manager: LocalWorldManager<E>,
    heartbeat_timer: Timer,
    timeout_timer: Timer,
    ack_manager: AckManager,
}

impl<E: Copy + Eq + Hash + Send + Sync> BaseConnection<E> {
    /// Create a new BaseConnection, given the appropriate underlying managers
    pub fn new(
        address: &Option<SocketAddr>,
        host_type: HostType,
        user_key: u64,
        connection_config: &ConnectionConfig,
        channel_kinds: &ChannelKinds,
        global_world_manager: &dyn GlobalWorldManagerType<E>,
    ) -> Self {
        BaseConnection {
            heartbeat_timer: Timer::new(connection_config.heartbeat_interval),
            timeout_timer: Timer::new(connection_config.disconnection_timeout_duration),
            ack_manager: AckManager::new(),
            message_manager: MessageManager::new(host_type, channel_kinds),
            host_world_manager: HostWorldManager::new(address, global_world_manager),
            remote_world_manager: RemoteWorldManager::new(),
            remote_world_reader: RemoteWorldReader::new(),
            local_world_manager: LocalWorldManager::new(user_key),
        }
    }

    // Heartbeats

    /// Record that a message has been sent (to prevent needing to send a
    /// heartbeat)
    pub fn mark_sent(&mut self) {
        self.heartbeat_timer.reset()
    }

    /// Returns whether a heartbeat message should be sent
    pub fn should_send_heartbeat(&self) -> bool {
        self.heartbeat_timer.ringing()
    }

    // Timeouts

    /// Record that a message has been received from a remote host (to prevent
    /// disconnecting from the remote host)
    pub fn mark_heard(&mut self) {
        self.timeout_timer.reset()
    }

    /// Returns whether this connection should be dropped as a result of a
    /// timeout
    pub fn should_drop(&self) -> bool {
        self.timeout_timer.ringing()
    }

    // Acks & Headers

    /// Process an incoming packet, pulling out the packet index number to keep
    /// track of the current RTT, and sending the packet to the AckManager to
    /// handle packet notification events
    pub fn process_incoming_header(
        &mut self,
        header: &StandardHeader,
        packet_notifiables: &mut [&mut dyn PacketNotifiable],
    ) {
        self.ack_manager.process_incoming_header(
            header,
            &mut self.message_manager,
            &mut self.host_world_manager,
            &mut self.local_world_manager,
            packet_notifiables,
        );
    }

    /// Given a packet payload, start tracking the packet via it's index, attach
    /// the appropriate header, and return the packet's resulting underlying
    /// bytes
    pub fn write_outgoing_header(&mut self, packet_type: PacketType, writer: &mut BitWriter) {
        // Add header onto message!
        self.ack_manager
            .next_outgoing_packet_header(packet_type)
            .ser(writer);
    }

    /// Get the next outgoing packet's index
    pub fn next_packet_index(&self) -> PacketIndex {
        self.ack_manager.next_sender_packet_index()
    }

    pub fn collect_outgoing_messages(&mut self, now: &Instant, rtt_millis: &f32) {
        self.host_world_manager
            .collect_outgoing_messages(rtt_millis);
        self.message_manager
            .collect_outgoing_messages(now, rtt_millis);
    }

    fn write_messages(
        &mut self,
        protocol: &Protocol,
        global_world_manager: &dyn GlobalWorldManagerType<E>,
        writer: &mut BitWriter,
        packet_index: PacketIndex,
        has_written: &mut bool,
    ) {
        let mut converter =
            EntityConverterMut::new(global_world_manager, &mut self.local_world_manager);
        self.message_manager.write_messages(
            protocol,
            &mut converter,
            writer,
            packet_index,
            has_written,
        );
    }

    pub fn write_outgoing_packet<W: WorldRefType<E>>(
        &mut self,
        protocol: &Protocol,
        now: &Instant,
        writer: &mut BitWriter,
        packet_index: PacketIndex,
        world: &W,
        global_world_manager: &dyn GlobalWorldManagerType<E>,
        has_written: &mut bool,
        write_world_events: bool,
        host_world_events: &mut HostWorldEvents<E>,
    ) {
        // write messages
        {
            self.write_messages(
                &protocol,
                global_world_manager,
                writer,
                packet_index,
                has_written,
            );

            // finish messages
            writer.release_bits(1);
            false.ser(writer);
        }

        if write_world_events {
            HostWorldWriter::write_into_packet(
                &protocol.component_kinds,
                now,
                writer,
                &packet_index,
                world,
                global_world_manager,
                &mut self.local_world_manager,
                has_written,
                &mut self.host_world_manager,
                host_world_events,
            );
        }
    }

    pub fn despawn_all_remote_entities<W: WorldMutType<E>>(
        &mut self,
        global_world_manager: &mut dyn GlobalWorldManagerType<E>,
        world: &mut W,
    ) -> Vec<EntityEvent<E>> {
        let mut output = Vec::new();

        let remote_entities = self.local_world_manager.remote_entities();

        for entity in remote_entities {
            // Generate remove event for each component, handing references off just in
            // case
            for component_kind in world.component_kinds(&entity) {
                if let Some(component) = world.remove_component_of_kind(&entity, &component_kind) {
                    output.push(EntityEvent::<E>::RemoveComponent(entity, component));
                }
            }

            // Despawn from global world manager
            global_world_manager.remote_despawn_entity(&entity);

            // Generate despawn event
            output.push(EntityEvent::DespawnEntity(entity));

            // Despawn entity
            world.despawn_entity(&entity);
        }

        output
    }
}
