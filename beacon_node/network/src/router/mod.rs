//! This module handles incoming network messages.
//!
//! It routes the messages to appropriate services, such as the Sync
//! and processes those that are
#![allow(clippy::unit_arg)]

pub mod processor;

use crate::error;
use crate::service::NetworkMessage;
use beacon_chain::{BeaconChain, BeaconChainTypes, BlockError};
use eth2_libp2p::{
    rpc::{RPCCodedResponse, RPCRequest, RPCResponse, RequestId, ResponseTermination},
    MessageId, NetworkGlobals, PeerId, PubsubMessage, RPCEvent,
};
use futures::prelude::*;
use processor::Processor;
use slog::{debug, info, o, trace, warn};
use std::sync::Arc;
use tokio::sync::mpsc;
use types::EthSpec;

/// Handles messages received from the network and client and organises syncing. This
/// functionality of this struct is to validate an decode messages from the network before
/// passing them to the internal message processor. The message processor spawns a syncing thread
/// which manages which blocks need to be requested and processed.
pub struct Router<T: BeaconChainTypes> {
    /// A channel to the network service to allow for gossip propagation.
    network_send: mpsc::UnboundedSender<NetworkMessage<T::EthSpec>>,
    /// Access to the peer db for logging.
    network_globals: Arc<NetworkGlobals<T::EthSpec>>,
    /// Processes validated and decoded messages from the network. Has direct access to the
    /// sync manager.
    processor: Processor<T>,
    /// The `Router` logger.
    log: slog::Logger,
}

/// Types of messages the handler can receive.
#[derive(Debug)]
pub enum RouterMessage<T: EthSpec> {
    /// We have initiated a connection to a new peer.
    PeerDialed(PeerId),
    /// Peer has disconnected,
    PeerDisconnected(PeerId),
    /// An RPC response/request has been received.
    RPC(PeerId, RPCEvent<T>),
    /// A gossip message has been received. The fields are: message id, the peer that sent us this
    /// message and the message itself.
    PubsubMessage(MessageId, PeerId, PubsubMessage<T>),
    /// The peer manager has requested we re-status a peer.
    StatusPeer(PeerId),
}

impl<T: BeaconChainTypes> Router<T> {
    /// Initializes and runs the Router.
    pub fn spawn(
        beacon_chain: Arc<BeaconChain<T>>,
        network_globals: Arc<NetworkGlobals<T::EthSpec>>,
        network_send: mpsc::UnboundedSender<NetworkMessage<T::EthSpec>>,
        runtime_handle: &tokio::runtime::Handle,
        log: slog::Logger,
    ) -> error::Result<mpsc::UnboundedSender<RouterMessage<T::EthSpec>>> {
        let message_handler_log = log.new(o!("service"=> "router"));
        trace!(message_handler_log, "Service starting");

        let (handler_send, handler_recv) = mpsc::unbounded_channel();

        // Initialise a message instance, which itself spawns the syncing thread.
        let processor = Processor::new(
            runtime_handle,
            beacon_chain,
            network_globals.clone(),
            network_send.clone(),
            &log,
        );

        // generate the Message handler
        let mut handler = Router {
            network_send,
            network_globals,
            processor,
            log: message_handler_log,
        };

        // spawn handler task and move the message handler instance into the spawned thread
        runtime_handle.spawn(async move {
            handler_recv
                .for_each(move |msg| future::ready(handler.handle_message(msg)))
                .await;
            debug!(log, "Network message handler terminated.");
        });

        Ok(handler_send)
    }

    /// Handle all messages incoming from the network service.
    fn handle_message(&mut self, message: RouterMessage<T::EthSpec>) {
        match message {
            // we have initiated a connection to a peer or the peer manager has requested a
            // re-status
            RouterMessage::PeerDialed(peer_id) | RouterMessage::StatusPeer(peer_id) => {
                self.processor.send_status(peer_id);
            }
            // A peer has disconnected
            RouterMessage::PeerDisconnected(peer_id) => {
                self.processor.on_disconnect(peer_id);
            }
            // An RPC message request/response has been received
            RouterMessage::RPC(peer_id, rpc_event) => {
                self.handle_rpc_message(peer_id, rpc_event);
            }
            // An RPC message request/response has been received
            RouterMessage::PubsubMessage(id, peer_id, gossip) => {
                self.handle_gossip(id, peer_id, gossip);
            }
        }
    }

    /* RPC - Related functionality */

    /// Handle RPC messages
    fn handle_rpc_message(&mut self, peer_id: PeerId, rpc_message: RPCEvent<T::EthSpec>) {
        match rpc_message {
            RPCEvent::Request(id, req) => self.handle_rpc_request(peer_id, id, req),
            RPCEvent::Response(id, resp) => self.handle_rpc_response(peer_id, id, resp),
            RPCEvent::Error(id, _protocol, error) => {
                warn!(self.log, "RPC Error"; "peer_id" => peer_id.to_string(), "request_id" => id, "error" => error.to_string(),
                    "client" => self.network_globals.client(&peer_id).to_string());
                self.processor.on_rpc_error(peer_id, id);
            }
        }
    }

    /// A new RPC request has been received from the network.
    fn handle_rpc_request(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        request: RPCRequest<T::EthSpec>,
    ) {
        match request {
            RPCRequest::Status(status_message) => {
                self.processor
                    .on_status_request(peer_id, request_id, status_message)
            }
            RPCRequest::Goodbye(goodbye_reason) => {
                debug!(
                    self.log, "Peer sent Goodbye";
                    "peer_id" => peer_id.to_string(),
                    "reason" => format!("{:?}", goodbye_reason),
                    "client" => self.network_globals.client(&peer_id).to_string(),
                );
                self.processor.on_disconnect(peer_id);
            }
            RPCRequest::BlocksByRange(request) => self
                .processor
                .on_blocks_by_range_request(peer_id, request_id, request),
            RPCRequest::BlocksByRoot(request) => self
                .processor
                .on_blocks_by_root_request(peer_id, request_id, request),
            RPCRequest::Ping(_) => unreachable!("Ping MUST be handled in the behaviour"),
            RPCRequest::MetaData(_) => unreachable!("MetaData MUST be handled in the behaviour"),
        }
    }

    /// An RPC response has been received from the network.
    // we match on id and ignore responses past the timeout.
    fn handle_rpc_response(
        &mut self,
        peer_id: PeerId,
        request_id: RequestId,
        error_response: RPCCodedResponse<T::EthSpec>,
    ) {
        // an error could have occurred.
        match error_response {
            RPCCodedResponse::InvalidRequest(error) => {
                warn!(self.log, "RPC Invalid Request";
                    "peer_id" => peer_id.to_string(),
                    "request_id" => request_id,
                    "error" => error.to_string(),
                    "client" => self.network_globals.client(&peer_id).to_string());
                self.processor.on_rpc_error(peer_id, request_id);
            }
            RPCCodedResponse::ServerError(error) => {
                warn!(self.log, "RPC Server Error" ;
                    "peer_id" => peer_id.to_string(),
                    "request_id" => request_id,
                    "error" => error.to_string(),
                    "client" => self.network_globals.client(&peer_id).to_string());
                self.processor.on_rpc_error(peer_id, request_id);
            }
            RPCCodedResponse::Unknown(error) => {
                warn!(self.log, "RPC Unknown Error";
                    "peer_id" => peer_id.to_string(),
                    "request_id" => request_id,
                    "error" => error.to_string(),
                    "client" => self.network_globals.client(&peer_id).to_string());
                self.processor.on_rpc_error(peer_id, request_id);
            }
            RPCCodedResponse::Success(response) => match response {
                RPCResponse::Status(status_message) => {
                    self.processor.on_status_response(peer_id, status_message);
                }
                RPCResponse::BlocksByRange(beacon_block) => {
                    self.processor.on_blocks_by_range_response(
                        peer_id,
                        request_id,
                        Some(beacon_block),
                    );
                }
                RPCResponse::BlocksByRoot(beacon_block) => {
                    self.processor.on_blocks_by_root_response(
                        peer_id,
                        request_id,
                        Some(beacon_block),
                    );
                }
                RPCResponse::Pong(_) => {
                    unreachable!("Ping must be handled in the behaviour");
                }
                RPCResponse::MetaData(_) => {
                    unreachable!("Meta data must be handled in the behaviour");
                }
            },
            RPCCodedResponse::StreamTermination(response_type) => {
                // have received a stream termination, notify the processing functions
                match response_type {
                    ResponseTermination::BlocksByRange => {
                        self.processor
                            .on_blocks_by_range_response(peer_id, request_id, None);
                    }
                    ResponseTermination::BlocksByRoot => {
                        self.processor
                            .on_blocks_by_root_response(peer_id, request_id, None);
                    }
                }
            }
        }
    }

    /// Handle RPC messages
    fn handle_gossip(
        &mut self,
        id: MessageId,
        peer_id: PeerId,
        gossip_message: PubsubMessage<T::EthSpec>,
    ) {
        match gossip_message {
            // Attestations should never reach the router.
            PubsubMessage::AggregateAndProofAttestation(aggregate_and_proof) => {
                if let Some(gossip_verified) =
                    self.processor.verify_aggregated_attestation_for_gossip(
                        peer_id.clone(),
                        *aggregate_and_proof.clone(),
                    )
                {
                    self.propagate_message(id, peer_id.clone());
                    self.processor
                        .import_aggregated_attestation(peer_id, gossip_verified);
                }
            }
            PubsubMessage::Attestation(subnet_attestation) => {
                if let Some(gossip_verified) =
                    self.processor.verify_unaggregated_attestation_for_gossip(
                        peer_id.clone(),
                        subnet_attestation.1.clone(),
                    )
                {
                    self.propagate_message(id, peer_id.clone());
                    self.processor
                        .import_unaggregated_attestation(peer_id, gossip_verified);
                }
            }
            PubsubMessage::BeaconBlock(block) => {
                match self.processor.should_forward_block(&peer_id, block) {
                    Ok(verified_block) => {
                        info!(self.log, "New block received"; "slot" => verified_block.block.slot(), "hash" => verified_block.block_root.to_string());
                        self.propagate_message(id, peer_id.clone());
                        self.processor.on_block_gossip(peer_id, verified_block);
                    }
                    Err(BlockError::ParentUnknown { .. }) => {} // performing a parent lookup
                    Err(e) => {
                        // performing a parent lookup
                        warn!(self.log, "Could not verify block for gossip";
                            "error" => format!("{:?}", e));
                    }
                }
            }
            PubsubMessage::VoluntaryExit(_exit) => {
                // TODO: Apply more sophisticated validation
                self.propagate_message(id, peer_id.clone());
                // TODO: Handle exits
                debug!(self.log, "Received a voluntary exit"; "peer_id" => format!("{}", peer_id) );
            }
            PubsubMessage::ProposerSlashing(_proposer_slashing) => {
                // TODO: Apply more sophisticated validation
                self.propagate_message(id, peer_id.clone());
                // TODO: Handle proposer slashings
                debug!(self.log, "Received a proposer slashing"; "peer_id" => format!("{}", peer_id) );
            }
            PubsubMessage::AttesterSlashing(_attester_slashing) => {
                // TODO: Apply more sophisticated validation
                self.propagate_message(id, peer_id.clone());
                // TODO: Handle attester slashings
                debug!(self.log, "Received an attester slashing"; "peer_id" => format!("{}", peer_id) );
            }
        }
    }

    /// Informs the network service that the message should be forwarded to other peers.
    fn propagate_message(&mut self, message_id: MessageId, propagation_source: PeerId) {
        self.network_send
            .send(NetworkMessage::Propagate {
                propagation_source,
                message_id,
            })
            .unwrap_or_else(|_| {
                warn!(
                    self.log,
                    "Could not send propagation request to the network service"
                )
            });
    }
}
