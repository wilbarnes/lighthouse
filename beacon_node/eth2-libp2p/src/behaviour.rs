use crate::discovery::Discovery;
use crate::rpc::{RPCEvent, RPCMessage, Rpc};
use crate::NetworkConfig;
use crate::{Topic, TopicHash};
use futures::prelude::*;
use libp2p::{
    core::{
        swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess},
        PublicKey,
    },
    gossipsub::{Gossipsub, GossipsubEvent},
    identify::{protocol::IdentifyInfo, Identify, IdentifyEvent},
    kad::KademliaOut,
    ping::{Ping, PingEvent},
    tokio_io::{AsyncRead, AsyncWrite},
    NetworkBehaviour, PeerId,
};
use slog::{debug, o, trace, warn};
use ssz::{ssz_encode, Decodable, DecodeError, Encodable, SszStream};
use types::{Attestation, BeaconBlock};

/// Builds the network behaviour for the libp2p Swarm.
/// Implements gossipsub message routing.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent", poll_method = "poll")]
pub struct Behaviour<TSubstream: AsyncRead + AsyncWrite> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub<TSubstream>,
    /// The events generated by this behaviour to be consumed in the swarm poll.
    serenity_rpc: Rpc<TSubstream>,
    /// Allows discovery of IP addresses for peers on the network.
    identify: Identify<TSubstream>,
    /// Keep regular connection to peers and disconnect if absent.
    ping: Ping<TSubstream>,
    /// Kademlia for peer discovery.
    discovery: Discovery<TSubstream>,
    /// Queue of behaviour events to be processed.
    #[behaviour(ignore)]
    events: Vec<BehaviourEvent>,
    /// Logger for behaviour actions.
    #[behaviour(ignore)]
    log: slog::Logger,
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<GossipsubEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(gs_msg) => {
                trace!(self.log, "Received GossipEvent"; "msg" => format!("{:?}", gs_msg));

                let pubsub_message = match PubsubMessage::ssz_decode(&gs_msg.data, 0) {
                    //TODO: Punish peer on error
                    Err(e) => {
                        warn!(
                            self.log,
                            "Received undecodable message from Peer {:?} error", gs_msg.source;
                            "error" => format!("{:?}", e)
                        );
                        return;
                    }
                    Ok((msg, _index)) => msg,
                };

                self.events.push(BehaviourEvent::GossipMessage {
                    source: gs_msg.source,
                    topics: gs_msg.topics,
                    message: pubsub_message,
                });
            }
            GossipsubEvent::Subscribed {
                peer_id: _,
                topic: _,
            }
            | GossipsubEvent::Unsubscribed {
                peer_id: _,
                topic: _,
            } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<RPCMessage>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: RPCMessage) {
        match event {
            RPCMessage::PeerDialed(peer_id) => {
                self.events.push(BehaviourEvent::PeerDialed(peer_id))
            }
            RPCMessage::RPC(peer_id, rpc_event) => {
                self.events.push(BehaviourEvent::RPC(peer_id, rpc_event))
            }
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<IdentifyEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Identified {
                peer_id, mut info, ..
            } => {
                if info.listen_addrs.len() > 20 {
                    debug!(
                        self.log,
                        "More than 20 peers have been identified, truncating"
                    );
                    info.listen_addrs.truncate(20);
                }
                trace!(self.log, "Found addresses"; "Peer Id" => format!("{:?}", peer_id), "Addresses" => format!("{:?}", info.listen_addrs));
                // inject the found addresses into our discovery behaviour
                for address in &info.listen_addrs {
                    self.discovery
                        .add_connected_address(&peer_id, address.clone());
                }
                self.events.push(BehaviourEvent::Identified(peer_id, info));
            }
            IdentifyEvent::Error { .. } => {}
            IdentifyEvent::SendBack { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<PingEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: PingEvent) {
        // not interested in ping responses at the moment.
    }
}

// implement the discovery behaviour (currently kademlia)
impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<KademliaOut>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _out: KademliaOut) {
        // not interested in kademlia results at the moment
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    pub fn new(local_public_key: PublicKey, net_conf: &NetworkConfig, log: &slog::Logger) -> Self {
        let local_peer_id = local_public_key.clone().into_peer_id();
        let identify_config = net_conf.identify_config.clone();
        let behaviour_log = log.new(o!());

        Behaviour {
            serenity_rpc: Rpc::new(log),
            gossipsub: Gossipsub::new(local_peer_id.clone(), net_conf.gs_config.clone()),
            discovery: Discovery::new(local_peer_id, log),
            identify: Identify::new(
                identify_config.version,
                identify_config.user_agent,
                local_public_key,
            ),
            ping: Ping::new(),
            events: Vec::new(),
            log: behaviour_log,
        }
    }

    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
    ) -> Async<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent>> {
        if !self.events.is_empty() {
            return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Async::NotReady
    }
}

/// Implements the combined behaviour for the libp2p service.
impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /// Subscribes to a gossipsub topic.
    pub fn subscribe(&mut self, topic: Topic) -> bool {
        self.gossipsub.subscribe(topic)
    }

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.serenity_rpc.send_rpc(peer_id, rpc_event);
    }

    /// Publishes a message on the pubsub (gossipsub) behaviour.
    pub fn publish(&mut self, topics: Vec<Topic>, message: PubsubMessage) {
        let message_bytes = ssz_encode(&message);
        for topic in topics {
            self.gossipsub.publish(topic, message_bytes.clone());
        }
    }
}

/// The types of events than can be obtained from polling the behaviour.
pub enum BehaviourEvent {
    RPC(PeerId, RPCEvent),
    PeerDialed(PeerId),
    Identified(PeerId, IdentifyInfo),
    // TODO: This is a stub at the moment
    GossipMessage {
        source: PeerId,
        topics: Vec<TopicHash>,
        message: PubsubMessage,
    },
}

/// Messages that are passed to and from the pubsub (Gossipsub) behaviour.
#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    Block(BeaconBlock),
    /// Gossipsub message providing notification of a new attestation.
    Attestation(Attestation),
}

//TODO: Correctly encode/decode enums. Prefixing with integer for now.
impl Encodable for PubsubMessage {
    fn ssz_append(&self, s: &mut SszStream) {
        match self {
            PubsubMessage::Block(block_gossip) => {
                0u32.ssz_append(s);
                block_gossip.ssz_append(s);
            }
            PubsubMessage::Attestation(attestation_gossip) => {
                1u32.ssz_append(s);
                attestation_gossip.ssz_append(s);
            }
        }
    }
}

impl Decodable for PubsubMessage {
    fn ssz_decode(bytes: &[u8], index: usize) -> Result<(Self, usize), DecodeError> {
        let (id, index) = u32::ssz_decode(bytes, index)?;
        match id {
            0 => {
                let (block, index) = BeaconBlock::ssz_decode(bytes, index)?;
                Ok((PubsubMessage::Block(block), index))
            }
            1 => {
                let (attestation, index) = Attestation::ssz_decode(bytes, index)?;
                Ok((PubsubMessage::Attestation(attestation), index))
            }
            _ => Err(DecodeError::Invalid),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use types::*;

    #[test]
    fn ssz_encoding() {
        let original = PubsubMessage::Block(BeaconBlock::empty(&ChainSpec::foundation()));

        let encoded = ssz_encode(&original);

        println!("{:?}", encoded);

        let (decoded, _i) = PubsubMessage::ssz_decode(&encoded, 0).unwrap();

        assert_eq!(original, decoded);
    }
}
