// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    application::storage::PeersAndMetadata,
    application::metadata::ConnectionState,
    // application::{metadata::ConnectionState, storage::{PeersAndMetadata,DisconnectReason}},
    // peer_manager::{ConnectionNotification, PeerManagerNotification, PeerManagerRequest},
    // protocols::{
    //     direct_send::Message,
    //     // rpc::{InboundRpcRequest, OutboundRpcRequest},
    // },
    protocols::network::ReceivedMessage, // wire::messaging::v1::NetworkMessage},
    transport::ConnectionMetadata,
    ProtocolId,
};
use aptos_config::{
    config::{PeerRole, RoleType},
    network_id::{NetworkId, PeerNetworkId},
};
use aptos_netcore::transport::ConnectionOrigin;
use aptos_types::PeerId;
use async_trait::async_trait;
// use futures::StreamExt;
use std::{collections::HashMap, sync::Arc};
use crate::protocols::network::OutboundPeerConnections;

/// A sender to a node to mock an inbound network message from [`PeerManager`]
pub type InboundMessageSender = tokio::sync::mpsc::Sender<ReceivedMessage>;
//     // aptos_channels::aptos_channel::Sender<(PeerId, ProtocolId), PeerManagerNotification>;

// /// A sender to a node to mock an inbound connection from [`PeerManager`]
// pub type ConnectionUpdateSender = crate::peer_manager::conn_notifs_channel::Sender;

/// A receiver to get outbound network messages to some peer
/// Spy on things being sent by application code through a NetworkSender
pub type OutboundMessageReceiver = tokio::sync::mpsc::Receiver<ReceivedMessage>;
//     aptos_channels::aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerRequest>;

/// A connection handle describing the network for a node.
///
/// Use this to interact with the node
#[derive(Clone)]
pub struct InboundNetworkHandle {
    /// To send new incoming network messages
    pub inbound_message_sender: InboundMessageSender,
    // /// To send new incoming connections or disconnections
    // pub connection_update_sender: ConnectionUpdateSender,
    /// To update the local state (normally done by peer manager)
    pub peers_and_metadata: Arc<PeersAndMetadata>,
}

// #[cfg(obsolete)]
impl InboundNetworkHandle {
    /// Push connection update, and update the local storage
    pub fn connect(
        &self,
        _role: RoleType,// TODO: use in logging?
        self_peer_network_id: PeerNetworkId,
        conn_metadata: ConnectionMetadata,
    ) {
        // PeerManager pushes this data before it's received by events
        let network_id = self_peer_network_id.network_id();
        let peer_id = conn_metadata.remote_peer_id;
        self.peers_and_metadata
            .insert_connection_metadata(
                PeerNetworkId::new(network_id, peer_id),
                conn_metadata.clone(),
            )
            .unwrap();

        // let self_peer_id = self_peer_network_id.peer_id();
        // self.connection_update_sender
        //     .push(
        //         conn_metadata.remote_peer_id,
        //         ConnectionNotification::NewPeer(
        //             conn_metadata,
        //             NetworkContext::new(role, network_id, self_peer_id),
        //         ),
        //     )
        //     .unwrap();
    }

    /// Push disconnect update, and update the local storage
    pub fn disconnect(
        &self,
        _role: RoleType, // TODO: use in logging?
        self_peer_network_id: PeerNetworkId,
        conn_metadata: ConnectionMetadata,
    ) {
        // let self_peer_id = self_peer_network_id.peer_id();
        let network_id = self_peer_network_id.network_id();

        // Set the state of the peer as disconnected
        let peer_network_id = PeerNetworkId::new(network_id, conn_metadata.remote_peer_id);
        self.peers_and_metadata
            .update_connection_state(peer_network_id, ConnectionState::Disconnected)
            .unwrap();

        // Push the notification of the lost peer
        // self.connection_update_sender
        //     .push(
        //         conn_metadata.remote_peer_id,
        //         ConnectionNotification::LostPeer(
        //             conn_metadata,
        //             NetworkContext::new(role, network_id, self_peer_id),
        //             DisconnectReason::ConnectionLost,
        //         ),
        //     )
        //     .unwrap();
    }
}

/// A unique identifier of a node across the entire network
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId {
    pub owner: u32,
    pub node_type: NodeType,
}

impl NodeId {
    pub fn validator(owner: u32) -> Self {
        Self {
            owner,
            node_type: NodeType::Validator,
        }
    }

    pub fn vfn(owner: u32) -> Self {
        Self {
            owner,
            node_type: NodeType::ValidatorFullNode,
        }
    }

    pub fn pfn(owner: u32) -> Self {
        Self {
            owner,
            node_type: NodeType::PublicFullNode,
        }
    }

    pub fn role(&self) -> RoleType {
        match self.node_type {
            NodeType::Validator => RoleType::Validator,
            _ => RoleType::FullNode,
        }
    }

    pub fn peer_role(&self) -> PeerRole {
        match self.node_type {
            NodeType::Validator => PeerRole::Validator,
            NodeType::ValidatorFullNode => PeerRole::ValidatorFullNode,
            NodeType::PublicFullNode => PeerRole::Unknown,
        }
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{:?}", self.owner, self.node_type)
    }
}

/// An enum defining the type of node
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeType {
    Validator,
    ValidatorFullNode,
    PublicFullNode,
}

/// A trait defining an application specific node with networking abstracted
///
/// This is built as an abstract implementation of networking around a node
#[async_trait]
pub trait ApplicationNode {
    fn node_id(&self) -> NodeId;

    /// Default ['ProtocolId`]s to connect with
    fn default_protocols(&self) -> &[ProtocolId];

    /// For sending to this node. Generally should not be used after setup
    fn get_inbound_handle(&self, network_id: NetworkId) -> InboundNetworkHandle;

    /// For adding handles to other peers
    fn add_inbound_handle_for_peer(
        &mut self,
        peer_network_id: PeerNetworkId,
        handle: InboundNetworkHandle,
    );

    /// For sending to other nodes
    fn get_inbound_handle_for_peer(&self, peer_network_id: PeerNetworkId) -> InboundNetworkHandle;

    /// For receiving messages from other nodes
    fn get_outbound_handle(&mut self, network_id: NetworkId) -> &mut Arc<OutboundPeerConnections>;

    // async fn get_next_network_msg(&mut self, network_id: NetworkId) -> (PeerId,NetworkMessage);

    fn get_peers_and_metadata(&self) -> &PeersAndMetadata;

    fn peer_network_ids(&self) -> &HashMap<NetworkId, PeerNetworkId>;
}

/// An extension trait for an `ApplicationNode` to run tests on.
///
/// Handles common implementation and helper functions
#[async_trait]
pub trait TestNode: ApplicationNode + Sync {
    /// Retrieve the [`PeerNetworkId`] for a specific [`NetworkId`].
    ///
    /// There can only be one per network.
    fn peer_network_id(&self, network_id: NetworkId) -> PeerNetworkId {
        *self.peer_network_ids().get(&network_id).unwrap_or_else(|| {
            panic!(
                "Expected network {} to exist on node {}",
                network_id,
                self.node_id()
            )
        })
    }

    /// Retrieve all [`NetworkId`] for the node
    fn network_ids(&self) -> Vec<NetworkId> {
        self.peer_network_ids().keys().copied().collect()
    }

    /// Connects a node to another node.  The other's inbound handle must already be added.
    fn connect(&self, network_id: NetworkId, metadata: ConnectionMetadata) {
        assert_eq!(ConnectionOrigin::Outbound, metadata.origin);
        let self_metadata = self.conn_metadata(network_id, ConnectionOrigin::Inbound, &[]);
        let remote_peer_id = metadata.remote_peer_id;

        // Tell the other node it's good to send to the connected peer now
        let remote_peer_network_id = PeerNetworkId::new(network_id, remote_peer_id);
        self.get_inbound_handle_for_peer(remote_peer_network_id)
            .connect(self.node_id().role(), remote_peer_network_id, self_metadata);

        // Then connect us
        self.connect_self(network_id, metadata);
    }

    /// Connects only the local side, useful for mocking the other node
    fn connect_self(&self, network_id: NetworkId, metadata: ConnectionMetadata) {
        self.get_inbound_handle(network_id).connect(
            self.node_id().role(),
            self.peer_network_id(network_id),
            metadata,
        );
    }

    /// Disconnects only the local side, useful for mocking the other node
    fn disconnect_self(&self, network_id: NetworkId, metadata: ConnectionMetadata) {
        self.get_inbound_handle(network_id).disconnect(
            self.node_id().role(),
            self.peer_network_id(network_id),
            metadata,
        );
    }

    /// Find a common [`NetworkId`] between nodes based on [`NodeType`]
    fn find_common_network(&self, other: &Self) -> Option<NetworkId> {
        let self_node_type = self.node_id().node_type;
        let other_node_type = other.node_id().node_type;
        match self_node_type {
            NodeType::Validator => match other_node_type {
                NodeType::Validator => Some(NetworkId::Validator),
                NodeType::ValidatorFullNode => Some(NetworkId::Vfn),
                NodeType::PublicFullNode => None,
            },
            NodeType::ValidatorFullNode => match other_node_type {
                NodeType::Validator => Some(NetworkId::Vfn),
                _ => Some(NetworkId::Public),
            },
            NodeType::PublicFullNode => match other_node_type {
                NodeType::Validator => None,
                _ => Some(NetworkId::Public),
            },
        }
    }

    /// Build `ConnectionMetadata` for a connection on another node
    fn conn_metadata(
        &self,
        network_id: NetworkId,
        origin: ConnectionOrigin,
        protocol_ids: &[ProtocolId],
    ) -> ConnectionMetadata {
        mock_conn_metadata(
            self.peer_network_id(network_id),
            self.node_id().peer_role(),
            origin,
            if protocol_ids.is_empty() {
                self.default_protocols()
            } else {
                protocol_ids
            },
        )
    }
}

/// Creates a [`ConnectionMetadata`].
pub fn mock_conn_metadata(
    peer_network_id: PeerNetworkId,
    peer_role: PeerRole,
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> ConnectionMetadata {
    let mut metadata =
        ConnectionMetadata::mock_with_role_and_origin(peer_network_id.peer_id(), peer_role, origin);
    for protocol_id in protocol_ids {
        metadata.application_protocols.insert(*protocol_id);
    }
    metadata
}

/// Creates a mock connection based on the `Validator` to `Validator` connection
pub fn validator_mock_connection(
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    mock_connection(
        NetworkId::Validator,
        PeerRole::Validator,
        origin,
        protocol_ids,
    )
}

/// Creates a mock connection based on the `Vfn` to `Validator` connection
pub fn vfn_validator_mock_connection(
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    let peer_role = match origin {
        ConnectionOrigin::Inbound => PeerRole::ValidatorFullNode,
        ConnectionOrigin::Outbound => PeerRole::Validator,
    };
    mock_connection(NetworkId::Vfn, peer_role, origin, protocol_ids)
}

/// Creates a mock connection based on the `Pfn` to `Vfn` connection
pub fn pfn_vfn_mock_connection(
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    let peer_role = match origin {
        ConnectionOrigin::Inbound => PeerRole::Unknown,
        ConnectionOrigin::Outbound => PeerRole::ValidatorFullNode,
    };
    mock_connection(NetworkId::Public, peer_role, origin, protocol_ids)
}

/// Creates a mock connection based on the `Vfn` to `Vfn` connection
pub fn vfn_vfn_mock_connection(
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    mock_connection(
        NetworkId::Public,
        PeerRole::ValidatorFullNode,
        origin,
        protocol_ids,
    )
}

/// Creates a mock connection based on the `Pfn` to `Pfn` connection
pub fn pfn_pfn_mock_connection(
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    mock_connection(NetworkId::Public, PeerRole::Known, origin, protocol_ids)
}

fn mock_connection(
    network_id: NetworkId,
    peer_role: PeerRole,
    origin: ConnectionOrigin,
    protocol_ids: &[ProtocolId],
) -> (PeerNetworkId, ConnectionMetadata) {
    let peer = PeerNetworkId::new(network_id, PeerId::random());
    let metadata = mock_conn_metadata(peer, peer_role, origin, protocol_ids);
    (peer, metadata)
}
