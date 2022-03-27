
use std::{
    collections::VecDeque,
    time::Duration,
};
use std::collections::HashMap;
use log::info;

use naia_serde::{BitReader, UnsignedVariableInteger};

use super::{
    message_list_header, channel_config::TickBufferSettings
};

use crate::{protocol::{entity_property::NetEntityHandleConverter, replicate::ReplicateSafe, protocolize::Protocolize, manifest::Manifest}, types::{Tick, MessageId}, serde::{BitCounter, BitWrite, BitWriter, Serde}, constants::{MTU_SIZE_BITS, MESSAGE_HISTORY_SIZE}, Instant, sequence_greater_than, sequence_less_than, wrapping_diff};

type ShortMessageId = u8;

pub struct ChannelTickBuffer<P: Protocolize> {
    sending_messages: OutgoingMessages<P>,
    next_send_messages: VecDeque<(Tick, Vec<(MessageId, P)>)>,
    resend_interval: Duration,
    last_sent: Instant,
    incoming_messages: IncomingMessages<P>,
}

impl<P: Protocolize> ChannelTickBuffer<P> {
    pub fn new(settings: &TickBufferSettings) -> Self {
        Self {
            sending_messages: OutgoingMessages::new(),
            next_send_messages: VecDeque::new(),
            resend_interval: settings.resend_interval.clone(),
            last_sent: Instant::now(),
            incoming_messages: IncomingMessages::new(),
        }
    }

    pub fn collect_incoming_messages(&mut self, tick: Tick) -> Vec<P> {
        let mut output = Vec::new();
        while let Some(message) = self.incoming_messages.pop_front(tick) {
            output.push(message);
        }
        return output;
    }

    pub fn collect_outgoing_messages(
        &mut self,
        client_sending_tick: &Tick,
        server_receivable_tick: &Tick,
    ) {
        if self.last_sent.elapsed() >= self.resend_interval {
            // Remove messages that would never be able to reach the Server
            self.sending_messages
                .pop_back_until_excluding(server_receivable_tick);

            self.last_sent = Instant::now();

            // Loop through outstanding messages and add them to the outgoing list
            let mut iter = self.sending_messages.iter();
            while let Some((message_tick, message_map)) = iter.next() {
                if sequence_greater_than(*message_tick, *client_sending_tick) {
                    info!("found message that is more recent than client sending tick! (how?)");
                    break;
                }
                let messages = message_map.collect_messages();
                self.next_send_messages.push_back((*message_tick, messages));
            }

            if self.next_send_messages.len() > 0 {
                info!("next_send_messages.len() = {} messages", self.next_send_messages.len());
            }
        }
    }

    pub fn send_message<R: ReplicateSafe<P>>(&mut self, host_tick: &Tick, message: &R) {
        let message_protocol = message.protocol_copy();

        self.sending_messages.push(*host_tick, message_protocol);

        self.last_sent = Instant::now();
        self.last_sent.subtract_duration(&self.resend_interval);
    }

    pub fn has_outgoing_messages(&self) -> bool {
        return self.next_send_messages.len() != 0;
    }

    // Tick Buffer Message Writing

    pub fn write_messages(
        &mut self,
        converter: &dyn NetEntityHandleConverter,
        writer: &mut BitWriter,
        host_tick: &Tick,
    ) -> Option<Vec<(Tick, MessageId)>> {
        let mut message_count: u16 = 0;

        // Header
        {
            // Measure
            let current_packet_size = writer.bit_count();
            if current_packet_size > MTU_SIZE_BITS {
                message_list_header::write(writer, 0);
                return None;
            }

            let mut counter = BitCounter::new();

            //TODO: message_count is inaccurate here and may be different than final, does
            // this matter?
            message_list_header::write(&mut counter, 123);

            // Check for overflow
            if current_packet_size + counter.bit_count() > MTU_SIZE_BITS {
                message_list_header::write(writer, 0);
                return None;
            }

            // Find how many messages will fit into the packet
            let mut last_written_tick = *host_tick;
            let mut index = 0;
            loop {
                if index >= self.next_send_messages.len() {
                    break;
                }

                let (message_tick, messages) = self.next_send_messages.get(index).unwrap();
                self.write_message(
                    converter,
                    &mut counter,
                    &last_written_tick,
                    message_tick,
                    messages,
                );
                last_written_tick = *message_tick;
                if current_packet_size + counter.bit_count() <= MTU_SIZE_BITS {
                    message_count += 1;
                } else {
                    break;
                }

                index += 1;
            }
        }

        // Write header
        message_list_header::write(writer, message_count);

        // Messages
        {
            let mut last_written_tick = *host_tick;
            let mut output = Vec::new();
            for _ in 0..message_count {
                // Pop message
                let (message_tick, messages) =
                    self.next_send_messages.pop_front().unwrap();

                // Write message
                let message_ids = self.write_message(
                    converter,
                    writer,
                    &last_written_tick,
                    &message_tick,
                    &messages,
                );
                last_written_tick = message_tick;
                for message_id in message_ids {
                    output.push((message_tick, message_id));
                }
            }
            return Some(output);
        }
    }

    /// Writes a Command into the Writer's internal buffer, which will
    /// eventually be put into the outgoing packet
    pub fn write_message<S: BitWrite>(
        &self,
        converter: &dyn NetEntityHandleConverter,
        writer: &mut S,
        last_written_tick: &Tick,
        message_tick: &Tick,
        messages: &Vec<(MessageId, P)>,
    ) -> Vec<MessageId> {

        let mut message_ids = Vec::new();

        // write message tick diff
        // this is reversed (diff is always negative, but it's encoded as positive)
        // because packet tick is always larger than past ticks
        let message_tick_diff = wrapping_diff(*message_tick, *last_written_tick);
        let message_tick_diff_encoded = UnsignedVariableInteger::<3>::new(message_tick_diff);
        message_tick_diff_encoded.ser(writer);

        // write number of messages
        let message_count = UnsignedVariableInteger::<3>::new(messages.len() as u64);
        message_count.ser(writer);

        for (message_id, message) in messages {

            message_ids.push(*message_id);

            // write message id
            let short_msg_id: u8 = (message_id % 256) as u8;
            short_msg_id.ser(writer);

            // write message kind
            message.dyn_ref().kind().ser(writer);

            // write payload
            message.write(writer, converter);
        }

        return message_ids;
    }

    pub fn notify_message_delivered(&mut self, tick: &Tick, message_id: &MessageId) {
        self.sending_messages.remove_message(tick, message_id);
    }

    pub fn read_messages(
        &mut self,
        host_tick: &Tick,
        remote_tick: &Tick,
        reader: &mut BitReader,
        manifest: &Manifest<P>,
        converter: &mut dyn NetEntityHandleConverter,
    ) {
        let mut last_read_tick = *remote_tick;
        let message_count = message_list_header::read(reader);
        for _x in 0..message_count {
            self.read_message(host_tick, &mut last_read_tick, reader, manifest, converter);
        }
    }

    /// Given incoming packet data, read transmitted Message and store
    /// them to be returned to the application
    fn read_message(
        &mut self,
        host_tick: &Tick,
        last_read_tick: &mut Tick,
        reader: &mut BitReader,
        manifest: &Manifest<P>,
        converter: &dyn NetEntityHandleConverter,
    ) {
        // read remote tick
        let last_remote_tick = *last_read_tick;
        let remote_tick_diff = UnsignedVariableInteger::<3>::de(reader).unwrap().get() as u16;
        let remote_tick = last_remote_tick.wrapping_sub(remote_tick_diff);
        *last_read_tick = last_remote_tick;

        // read message count
        let message_count = UnsignedVariableInteger::<3>::de(reader).unwrap().get();

        for _ in 0..message_count {
            // read message id
            let short_msg_id: ShortMessageId = ShortMessageId::de(reader).unwrap();

            // read message kind
            let replica_kind: P::Kind = P::Kind::de(reader).unwrap();

            // read payload
            let new_message = manifest.create_replica(replica_kind, reader, converter);

            if !self.incoming_messages.push_back(
                &remote_tick,
                host_tick,
                short_msg_id,
                new_message,
            ) {
                //info!("failed command. server: {}, client: {}",
                // server_tick, client_tick);
            }
        }
    }
}

// MessageMap
struct MessageMap<P: Protocolize> {
    list: Vec<Option<P>>,
}

impl<P: Protocolize> MessageMap<P> {
    pub fn new() -> Self {
        MessageMap {
            list: Vec::new(),
        }
    }

    pub fn insert(&mut self, message: P) {
        self.list.push(Some(message));
    }

    pub fn collect_messages(&self) -> Vec<(MessageId, P)> {
        let mut output = Vec::new();
        let mut index = 0;
        for message_opt in &self.list {
            if let Some(message) = message_opt {
                output.push((index, message.clone()));
            }
            index += 1;
        }
        return output;
    }

    pub fn remove(&mut self, message_id: &MessageId) {
        if let Some(container) = self.list.get_mut(*message_id as usize) {
            *container = None;
        }
    }

    pub fn len(&self) -> usize {
        return self.list.len();
    }
}

// OutgoingMessages

struct OutgoingMessages<P: Protocolize> {
    // front big, back small
    // front recent, back past
    buffer: VecDeque<(Tick, MessageMap<P>)>,
}

impl<P: Protocolize> OutgoingMessages<P> {
    pub fn new() -> Self {
        OutgoingMessages {
            buffer: VecDeque::new(),
        }
    }

    // should only push increasing ticks of messages
    pub fn push(&mut self, message_tick: Tick, message_protocol: P) {
        if let Some((front_tick, msg_map)) = self.buffer.front_mut() {
            if message_tick == *front_tick {
                // been here before, cool
                msg_map.insert(message_protocol);
                return;
            }

            if sequence_less_than(message_tick, *front_tick) {
                panic!("this method should always receive increasing or equal ticks!")
            }
        } else {
            // nothing is in here
        }

        let mut msg_map = MessageMap::new();
        msg_map.insert(message_protocol);
        self.buffer.push_front((message_tick, msg_map));

        // a good time to prune down this list
        while self.buffer.len() > MESSAGE_HISTORY_SIZE.into() {
            self.buffer.pop_back();
            info!("pruning outgoing_messages buffer cause it got too big");
        }
    }

    pub fn pop_back_until_excluding(&mut self, until_tick: &Tick) {
        loop {
            if let Some((old_tick, _)) = self.buffer.back() {
                if sequence_less_than(*until_tick, *old_tick) {
                    return;
                }
            } else {
                return;
            }

            self.buffer.pop_back();
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Tick, MessageMap<P>)> {
        self.buffer.iter()
    }

    pub fn remove_message(&mut self, tick: &Tick, msg_id: &MessageId) {
        let mut index = self.buffer.len();

        if index == 0 {
            // empty condition
            return;
        }

        loop {
            index -= 1;

            let mut remove = false;

            if let Some((old_tick, msg_map)) = self.buffer.get_mut(index) {
                if *old_tick == *tick {
                    // found it!
                    msg_map.remove(&msg_id);
                    //info!("removed delivered message! tick: {}, msg_id: {}",
                    // tick, msg_id);
                    if msg_map.len() == 0 {
                        remove = true;
                    }
                } else {
                    // if tick is less than old tick, no sense continuing, only going to get bigger
                    // as we go
                    if sequence_greater_than(*old_tick, *tick) {
                        return;
                    }
                }
            }

            if remove {
                self.buffer.remove(index);
            }

            if index == 0 {
                return;
            }
        }
    }
}

// Incoming messages

struct IncomingMessages<P: Protocolize> {
    // front is small, back is big
    buffer: VecDeque<(Tick, HashMap<ShortMessageId, P>)>,
}

impl<P: Protocolize> IncomingMessages<P> {
    pub fn new() -> Self {
        IncomingMessages {
            buffer: VecDeque::new(),
        }
    }

    pub fn push_back(
        &mut self,
        remote_tick: &Tick,
        host_tick: &Tick,
        short_msg_id: ShortMessageId,
        new_message: P,
    ) -> bool {
        if sequence_greater_than(*remote_tick, *host_tick) {
            let mut index = self.buffer.len();

            //in the case of empty vec
            if index == 0 {
                let mut map = HashMap::new();
                map.insert(short_msg_id, new_message);
                self.buffer.push_back((*remote_tick, map));
                //info!("msg server_tick: {}, client_tick: {}, for entity: {} ... (empty q)",
                // server_tick, client_tick, owned_entity);
                return true;
            }

            let mut insert = false;
            loop {
                index -= 1;

                if let Some((tick, command_map)) = self.buffer.get_mut(index) {
                    if *tick == *remote_tick {
                        if !command_map.contains_key(&short_msg_id) {
                            command_map.insert(short_msg_id, new_message);
                            //info!("inserting command at tick: {}", client_tick);
                            //info!("msg server_tick: {}, client_tick: {}, for entity: {} ... (map
                            // xzist)", server_tick, client_tick, owned_entity);
                            // inserted command into existing tick
                            return true;
                        } else {
                            return false;
                        }
                    } else {
                        if sequence_greater_than(*remote_tick, *tick) {
                            // incoming client tick is larger than found tick ...
                            insert = true;
                        }
                    }
                }

                if insert {
                    // found correct position to insert node
                    let mut map = HashMap::new();
                    map.insert(short_msg_id, new_message);
                    self.buffer.insert(index + 1, (*remote_tick, map));
                    //info!("msg server_tick: {}, client_tick: {}, for entity: {} ... (midbck
                    // insrt)", server_tick, client_tick, owned_entity);
                    return true;
                }

                if index == 0 {
                    //traversed the whole vec, push front
                    let mut map = HashMap::new();
                    map.insert(short_msg_id, new_message);
                    self.buffer.push_front((*remote_tick, map));
                    //info!("msg server_tick: {}, client_tick: {}, for entity: {} ... (front
                    // insrt)", server_tick, client_tick, owned_entity);
                    return true;
                }
            }
        } else {
            // command is too late to insert in incoming message queue
            return false;
        }
    }

    pub fn pop_front(&mut self, server_tick: Tick) -> Option<P> {
        // get rid of outdated commands
        loop {
            let mut pop = false;
            if let Some((front_tick, _)) = self.buffer.front() {
                if sequence_greater_than(server_tick, *front_tick) {
                    pop = true;
                }
            } else {
                return None;
            }
            if pop {
                self.buffer.pop_front();
            } else {
                break;
            }
        }

        // now get the newest applicable command
        let mut output = None;
        let mut pop = false;
        if let Some((front_tick, command_map)) = self.buffer.front_mut() {
            if *front_tick == server_tick {
                let mut any_msg_id: Option<ShortMessageId> = None;
                if let Some(any_msg_id_ref) = command_map.keys().next() {
                    any_msg_id = Some(*any_msg_id_ref);
                }
                if let Some(msg_id) = any_msg_id {
                    if let Some(message) = command_map.remove(&msg_id) {
                        output = Some(message);
                        // info!("popping message at tick: {}, for entity: {}",
                        // front_tick, any_entity);
                    }
                    if command_map.len() == 0 {
                        pop = true;
                    }
                }
            }
        }

        if pop {
            self.buffer.pop_front();
        }

        return output;
    }
}