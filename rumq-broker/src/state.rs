use std::{collections::VecDeque, result::Result, time::Instant};

use rumq_core::*;
use std::time::Duration;

use crate::router::RouterMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttConnectionStatus {
    Handshake,
    Connected,
    Disconnected,
}

#[derive(Debug)]
pub enum Error {
    /// Broker's error reply to client's connect packet
    Connect(ConnectReturnCode),
    /// Invalid state for a given operation
    InvalidState,
    /// Invalid topic
    InvalidTopic,
    /// Received a packet (ack) which isn't asked for
    Unsolicited,
    /// Last pingreq isn't acked
    AwaitPingResp,
    /// Received a wrong packet while waiting for another packet
    WrongPacket,
    /// Unsupported packet
    UnsupportedPacket(Packet),
    /// Unsupported qos
    UnsupportedQoS,
    /// Invalid client ID
    InvalidClientId,
    /// Disconnect received
    Disconnect(String),
}

/// `MqttState` saves the state of the mqtt connection. Methods will
/// just modify the state of the object without doing any network operations
/// Methods returns so that n/w methods or n/w eventloop can
/// operate directly. This abstracts the functionality better
/// so that it's easy to switch between synchronous code, tokio (or)
/// async/await
#[derive(Debug)]
pub struct MqttState {
    /// Connection status
    connection_status: MqttConnectionStatus,
    /// Keep alive
    keep_alive: Option<Duration>,
    /// Status of last ping
    await_pingresp: bool,
    /// Last incoming packet time
    last_incoming: Instant,
    /// Last outgoing packet time
    last_outgoing: Instant,
    /// Packet id of the last outgoing packet
    last_pkid: PacketIdentifier,
    /// Outgoing QoS 1 publishes which aren't acked yet
    outgoing_pub: VecDeque<Publish>,
}

impl MqttState {
    /// Creates new mqtt state. Same state should be used during a
    /// connection for persistent sessions while new state should
    /// instantiated for clean sessions
    pub fn new() -> Self {
        MqttState {
            connection_status: MqttConnectionStatus::Handshake,
            keep_alive: None,
            await_pingresp: false,
            last_incoming: Instant::now(),
            last_outgoing: Instant::now(),
            last_pkid: PacketIdentifier(0),
            outgoing_pub: VecDeque::new(),
        }
    }

    /// Consolidates handling of all outgoing mqtt packet logic. Returns a packet which should
    /// be put on to the network by the eventloop
    pub fn handle_outgoing_mqtt_packet(&mut self, message: RouterMessage) -> Result<Packet, Error> {
        let out = match message {
            RouterMessage::Publish(publish) => self.handle_outgoing_publish(publish),
            _ => unimplemented!(),
        };

        self.last_outgoing = Instant::now();
        Ok(out)
    }

    /// Consolidates handling of all incoming mqtt packets. Returns a `Packet` which for the
    /// user to consume and `Packet` which for the eventloop to put on the network
    /// E.g For incoming QoS1 publish packet, this method returns (Publish, Puback). Publish packet will
    /// be forwarded to user and Pubck packet will be written to network
    pub fn handle_incoming_mqtt_packet(
        &mut self,
        id: &str,
        packet: Packet,
    ) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        let out = match packet {
            Packet::Publish(publish) => self.handle_incoming_publish(publish.clone()),
            Packet::Puback(pkid) => self.handle_incoming_puback(pkid),
            Packet::Subscribe(subscribe) => self.handle_incoming_subscribe(id, subscribe),
            Packet::Pingreq => self.handle_incoming_pingreq(),
            Packet::Disconnect => self.handle_incoming_disconnect(id),
            _ => return Err(Error::UnsupportedPacket(packet)),
        };

        self.last_incoming = Instant::now();
        out
    }

    /// Adds next packet identifier to QoS 1 and 2 publish packets and returns
    /// it buy wrapping publish in packet
    fn handle_outgoing_publish(&mut self, publish: Publish) -> Packet {
        let publish = match publish.qos() {
            QoS::AtMostOnce => publish,
            QoS::AtLeastOnce | QoS::ExactlyOnce => self.add_packet_id_and_save(publish),
        };

        debug!(
            "Outgoing Publish. Topic = {:?}, Pkid = {:?}, Payload Size = {:?}",
            publish.topic_name(),
            publish.pkid(),
            publish.payload().len()
        );
        Packet::Publish(publish)
    }

    pub fn handle_incoming_connect(&mut self, packet: Packet) -> Result<(Connect, Packet), Error> {
        let mut connect = match packet {
            Packet::Connect(connect) => connect,
            packet => {
                error!("Invalid packet. Expecting connect. Received = {:?}", packet);
                self.connection_status = MqttConnectionStatus::Disconnected;
                return Err(Error::WrongPacket);
            }
        };

        // this broker expects a keepalive. 0 keepalives are promoted to 10 minutes
        if connect.keep_alive == 0 {
            warn!("0 keepalive. Promoting it to 10 minutes");
            connect.keep_alive = 10 * 60;
        }

        if connect.client_id.starts_with(' ') || connect.client_id.is_empty() {
            error!("Client id shouldn't start with space (or) shouldn't be emptys");
            return Err(Error::InvalidClientId);
        }

        self.connection_status = MqttConnectionStatus::Connected;
        let connack = connack(ConnectReturnCode::Accepted, false);
        // TODO: Handle session present
        let reply = Packet::Connack(connack);

        Ok((connect, reply))
    }

    fn handle_incoming_pingreq(&mut self) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        let packet = Packet::Pingresp;
        Ok((None, Some(packet)))
    }

    /// Results in a publish notification in all the QoS cases. Replys with an ack
    /// in case of QoS1 and Replys rec in case of QoS while also storing the message
    fn handle_incoming_publish(&mut self, publish: Publish) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        let qos = publish.qos();

        debug!(
            "Incoming Publish. Topic = {:?}, Pkid = {:?}, Payload Size = {:?}",
            publish.topic_name(),
            publish.pkid(),
            publish.payload().len()
        );

        if !valid_topic(publish.topic_name()) {
            error!("Invalid topic = {} on publish", publish.topic_name());
            return Err(Error::InvalidTopic);
        }

        match qos {
            QoS::AtMostOnce => {
                let routermessage = RouterMessage::Publish(publish);
                Ok((Some(routermessage), None))
            }
            QoS::AtLeastOnce => {
                let pkid = publish.pkid().unwrap();
                let packet = Packet::Puback(pkid);
                let routermessage = RouterMessage::Publish(publish);
                Ok((Some(routermessage), Some(packet)))
            }
            QoS::ExactlyOnce => Err(Error::UnsupportedQoS),
        }
    }

    /// Iterates through the list of stored publishes and removes the publish with the
    /// matching packet identifier. Removal is now a O(n) operation. This should be
    /// usually ok in case of acks due to ack ordering in normal conditions.
    fn handle_incoming_puback(&mut self, pkid: PacketIdentifier) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        match self.outgoing_pub.iter().position(|x| *x.pkid() == Some(pkid)) {
            Some(index) => {
                let _publish = self.outgoing_pub.remove(index).expect("Wrong index");

                let routermessage = None;
                let packet = None;
                Ok((routermessage, packet))
            }
            None => {
                error!("Unsolicited puback packet: {:?}", pkid);
                Err(Error::Unsolicited)
            }
        }
    }

    fn handle_incoming_subscribe(
        &mut self,
        id: &str,
        subscription: Subscribe,
    ) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        debug!("Subscribe. Topics = {:?}, Pkid = {:?}", subscription.topics(), subscription.pkid());

        let pkid = subscription.pkid();
        let id = id.to_owned();

        let mut router_subscription = empty_subscribe();
        let mut subscription_return_codes = Vec::new();
        for topic in subscription.topics().iter() {
            let qos = topic.qos();
            let qos = match qos {
                QoS::AtMostOnce | QoS::AtLeastOnce => *qos,
                QoS::ExactlyOnce => QoS::AtLeastOnce,
            };

            let topic = topic.topic_path();
            // we don't support wildcards yet
            let code = if valid_filter(topic) { SubscribeReturnCodes::Success(qos) } else { SubscribeReturnCodes::Failure };

            // add only successful subscriptions to router message
            if let SubscribeReturnCodes::Success(qos) = code {
                router_subscription.add(topic.clone(), qos);
            }

            subscription_return_codes.push(code);
        }

        let packet = Packet::Suback(suback(*pkid, subscription_return_codes));
        let routermessage = RouterMessage::Subscribe((id, subscription));
        Ok((Some(routermessage), Some(packet)))
    }

    fn handle_incoming_disconnect(&mut self, id: &str) -> Result<(Option<RouterMessage>, Option<Packet>), Error> {
        // TODO: Do will handling here
        Err(Error::Disconnect(id.to_owned()))
    }

    /// Add publish packet to the state and return the packet. This method clones the
    /// publish packet to save it to the state. This might not be a problem during low
    /// frequency/size data publishes but should ideally be `Arc`d while returning to
    /// prevent deep copy of large messages as this is anyway immutable after adding pkid
    fn add_packet_id_and_save(&mut self, mut publish: Publish) -> Publish {
        let publish = if *publish.pkid() == None {
            let pkid = self.next_pkid();
            publish.set_pkid(pkid);
            publish
        } else {
            publish
        };

        self.outgoing_pub.push_back(publish.clone());
        publish
    }

    /// Increment the packet identifier from the state and roll it when it reaches its max
    /// http://stackoverflow.com/questions/11115364/mqtt-messageid-practical-implementation
    fn next_pkid(&mut self) -> PacketIdentifier {
        let PacketIdentifier(mut pkid) = self.last_pkid;
        if pkid == 65_535 {
            pkid = 0;
        }
        self.last_pkid = PacketIdentifier(pkid + 1);
        self.last_pkid
    }
}

#[cfg(test)]
mod test {
    /*
    use std::{sync::Arc, thread, time::Duration};

    use super::{Error, MqttConnectionStatus, MqttState, Packet};
    use rumq_core::*;

    fn build_outgoing_publish(qos: QoS) -> Publish {
        Publish { dup: false, qos, retain: false, pkid: None, topic_name: "hello/world".to_owned(), payload: Arc::new(vec![1, 2, 3]) }
    }

    fn build_incoming_publish(qos: QoS, pkid: u16) -> Publish {
        Publish {
            dup: false,
            qos,
            retain: false,
            pkid: Some(PacketIdentifier(pkid)),
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        }
    }

    fn build_mqttstate() -> MqttState {
        MqttState::new()
    }

    #[test]
    fn next_pkid_roll() {
        let mut mqtt = build_mqttstate();
        let mut pkt_id = PacketIdentifier(0);

        for _ in 0..65536 {
            pkt_id = mqtt.next_pkid();
        }
        assert_eq!(PacketIdentifier(1), pkt_id);
    }
    */
}
