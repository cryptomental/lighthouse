#![cfg(test)]
use super::*;
use crate::rpc::{HelloMessage, RPCRequest};
use crate::NetworkConfig;
use enr::{Enr, EnrBuilder, NodeId};
use futures;
use libp2p::core::identity::Keypair;
use libp2p::discv5::Key;
use slog::{debug, error, o, Drain};
use slog_stdlog;
use types::{Epoch, Hash256, Slot};
use Service as LibP2PService;

fn setup_log() -> slog::Logger {
    slog::Logger::root(slog_stdlog::StdLog.fuse(), o!())
}

// Testing
// 1) Test gossipsub and rpc without discovery with just 2 nodes
// 1.1) Use libp2p's boot_nodes to connect 2 nodes
// 1.2) Send message on all of the subscribed topics
// 1.3) Send message on unsubscribed topics
// 1.4) Subscribe to a new topic
// 2) RPC communication between 2 nodes for every type of RPC message

fn build_config(port: u16, mut boot_nodes: Vec<Enr>, secret_key: Option<String>) -> NetworkConfig {
    let mut config = NetworkConfig::default();
    config.libp2p_port = port; // tcp port
    config.discovery_port = port; // udp port
    config.boot_nodes.append(&mut boot_nodes);
    config.secret_key_hex = secret_key;
    config.network_dir.push(port.to_string());
    config
}

fn build_libp2p_instance(
    port: u16,
    boot_nodes: Vec<Enr>,
    secret_key: Option<String>,
    log: slog::Logger,
) -> LibP2PService {
    let config = build_config(port, boot_nodes, secret_key);
    let network_log = log.new(o!("Service" => "Libp2p"));
    // launch libp2p service
    let libp2p_service = LibP2PService::new(config.clone(), network_log.clone()).unwrap();
    libp2p_service
}

fn get_enr(node: &LibP2PService) -> Enr {
    node.swarm.discovery().local_enr().clone()
}

// Returns kademlia log distance between two nodes
fn get_distance(node1: &NodeId, node2: &NodeId) -> Option<u64> {
    let node1: Key<NodeId> = node1.clone().into();
    node1.log2_distance(&node2.clone().into())
}

// Generate secret keys for given node + an additional bootstrap node for testing discovery.
// Bootstrap node is close (kbucket index > 253) to all other nodes.
fn generate_secret_keys(n: usize) -> Vec<Option<String>> {
    let mut keypairs: Vec<Keypair> = Vec::new();
    let bootstrap_keypair: Keypair = Keypair::generate_secp256k1();
    let bootstrap_node_id = EnrBuilder::new()
        .build(&bootstrap_keypair)
        .unwrap()
        .node_id()
        .clone();
    for _ in 0..n {
        loop {
            let keypair = Keypair::generate_secp256k1();
            let enr = EnrBuilder::new().build(&keypair).unwrap();
            let key = enr.node_id();
            let distance = get_distance(&bootstrap_node_id, key).unwrap();
            // Any distance greater than 253 is good enough for discovering nodes in one
            // complete query. TODO: need to verify
            if distance > 253 {
                keypairs.push(keypair);
                break;
            }
        }
    }
    keypairs.push(bootstrap_keypair);
    keypairs
        .into_iter()
        .map(|x| match x {
            Keypair::Secp256k1(kp) => Some(hex::encode(kp.secret().to_bytes())),
            _ => None,
        })
        .collect::<Vec<_>>()
}

// Constructs, connects and returns n libp2p peers without discovery.
fn build_nodes(n: usize, start_port: Option<u16>) -> Vec<LibP2PService> {
    let log = setup_log();
    let base_port = start_port.unwrap_or(9000);
    let mut nodes: Vec<LibP2PService> = (base_port..base_port + n as u16)
        .map(|p| build_libp2p_instance(p, vec![], None, log.clone()))
        .collect();
    let multiaddrs: Vec<Multiaddr> = nodes
        .iter()
        .map(|x| get_enr(&x).multiaddr()[1].clone())
        .collect();

    for i in 0..n {
        for j in i..n {
            if i != j {
                match libp2p::Swarm::dial_addr(&mut nodes[i].swarm, multiaddrs[j].clone()) {
                    Ok(()) => debug!(log, "Connected"),
                    Err(_) => error!(log, "Failed to connect"),
                };
            }
        }
    }
    nodes
}

#[test]
fn test_discovery() {
    let log = setup_log();
    let num_nodes = 8;
    let mut secret_keys = generate_secret_keys(num_nodes);
    let bootstrap_sk = secret_keys.pop().unwrap();
    let bootstrap_node = build_libp2p_instance(9000, vec![], bootstrap_sk, log.clone());
    let base_port = 9001;
    let mut nodes: Vec<LibP2PService> = secret_keys
        .into_iter()
        .enumerate()
        .map(|(i, sk)| {
            build_libp2p_instance(
                base_port + i as u16,
                vec![get_enr(&bootstrap_node)],
                sk,
                log.clone(),
            )
        })
        .collect();
    nodes.push(bootstrap_node);
    tokio::run(futures::future::poll_fn(move || -> Result<_, ()> {
        for node in nodes.iter_mut() {
            loop {
                match node.poll().unwrap() {
                    Async::Ready(Some(Libp2pEvent::PeerDialed(peer_id))) => {
                        println!(
                            "Node {} is connected to {} peers.",
                            node.swarm.discovery().local_enr().node_id(),
                            node.swarm.discovery().connected_peers(),
                        );
                        // return Ok(Async::Ready(()));
                    }
                    Async::Ready(Some(_)) => (),
                    Async::Ready(None) | Async::NotReady => break,
                }
            }
        }
        Ok(Async::NotReady)
    }))
}

// Test publishing of a message with a full mesh for the topic
#[test]
fn test_gossipsub_full_mesh_publish() {
    let num_nodes = 13; // mesh_n_high + 1
    let mut nodes = build_nodes(num_nodes, None);
    let mut publishing_node = nodes.pop().unwrap();
    let pubsub_message = PubsubMessage::Block(vec![0; 4]);
    let mut subscribed_count = 0;
    let mut received_count = 0;
    tokio::run(futures::future::poll_fn(move || -> Result<_, ()> {
        for node in nodes.iter_mut() {
            loop {
                match node.poll().unwrap() {
                    Async::Ready(Some(Libp2pEvent::PubsubMessage {
                        topics, message, ..
                    })) => {
                        // Assert topics are eth2 topics
                        assert!(topics
                            .clone()
                            .iter()
                            .all(|t| t.clone().into_string() == "/eth2/beacon_block/ssz"));

                        // Assert message received is the correct one
                        assert_eq!(message, pubsub_message.clone());
                        received_count += 1;
                        if received_count == num_nodes - 1 {
                            return Ok(Async::Ready(()));
                        }
                    }
                    _ => break,
                }
            }
        }
        loop {
            match publishing_node.poll().unwrap() {
                Async::Ready(Some(Libp2pEvent::PeerSubscribed(_, topic))) => {
                    // Received topics is one of subscribed eth2 topics
                    assert!(topic.clone().into_string().starts_with("/eth2/"));
                    // Publish on beacon block topic
                    if topic == TopicHash::from_raw("/eth2/beacon_block/ssz") {
                        subscribed_count += 1;
                        if subscribed_count == num_nodes - 1 {
                            publishing_node.swarm.publish(
                                &vec![Topic::new(topic.into_string())],
                                pubsub_message.clone(),
                            );
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(Async::NotReady)
    }))
}

// Test if gossipsub message are forwarded by nodes.
// Each mesh may contains multiple nodes. All nodes in a mesh are connected to all other nodes
// in the same mesh.
//                                Topology used in test
//            |------------------------------|    |-----------------------------|
//            | mesh1 nodes --- border node  |----| border node --- mesh2 nodes |
//            |------------------------------|    |-----------------------------|
//
// Publisher is part of mesh1 and connecter is part of mesh2.
// All nodes in mesh 2 should also receive published message.
#[test]
fn test_gossipsub_forward() {
    let log = setup_log();
    let mesh1_n = 6;
    let mesh2_n = 6;
    let mut mesh1 = build_nodes(mesh1_n, Some(9000));
    let mut mesh2 = build_nodes(mesh2_n, Some(9006));
    let border1 = mesh1.pop().unwrap();
    let mut border2 = mesh2.pop().unwrap();
    // Connect one node from each mesh
    match libp2p::Swarm::dial_addr(&mut border2.swarm, get_enr(&border1).multiaddr()[1].clone()) {
        Ok(()) => debug!(log, "Connected"),
        Err(_) => error!(log, "Failed to connect"),
    }

    let pubsub_message = PubsubMessage::Block(vec![0; 4]);
    let publishing_topic: String = "/eth2/beacon_block/ssz".into();
    let mut subscribed_count = 0;
    let mut received_count = 0;

    let mut border_nodes = vec![border1, border2];
    mesh1.append(&mut mesh2);
    tokio::run(futures::future::poll_fn(move || -> Result<_, ()> {
        for node in mesh1.iter_mut() {
            loop {
                match node.poll().unwrap() {
                    Async::Ready(Some(Libp2pEvent::PubsubMessage {
                        topics, message, ..
                    })) => {
                        // Assert topics are eth2 topics
                        assert!(topics
                            .clone()
                            .iter()
                            .all(|t| t.clone().into_string() == publishing_topic.clone()));

                        // Assert message received is the correct one
                        assert_eq!(message, pubsub_message.clone());
                        received_count += 1;
                        println!("Received count: {}", received_count);
                        // Test should succeed if received count is equal to
                        // total_nodes - 1 (publisher)
                        if received_count == (mesh1_n + mesh2_n - 1) {
                            return Ok(Async::Ready(()));
                        }
                    }
                    _ => break,
                }
            }
        }
        for node in border_nodes.iter_mut() {
            loop {
                match node.poll().unwrap() {
                    Async::Ready(Some(Libp2pEvent::PeerSubscribed(_, topic))) => {
                        // Received topics is one of subscribed eth2 topics
                        assert!(topic.clone().into_string().starts_with("/eth2/"));
                        if topic == TopicHash::from_raw(publishing_topic.clone()) {
                            subscribed_count += 1;
                            if subscribed_count >= mesh1_n + mesh2_n - 1 {
                                // println!("Publishing from pnode");
                                node.swarm.publish(
                                    &vec![Topic::new(topic.into_string())],
                                    pubsub_message.clone(),
                                );
                            }
                        }
                    }
                    Async::Ready(Some(Libp2pEvent::PubsubMessage { .. })) => {
                        received_count += 1;
                        if received_count == (mesh1_n + mesh2_n - 1) {
                            return Ok(Async::Ready(()));
                        }
                    }
                    _ => break,
                }
            }
        }
        Ok(Async::NotReady)
    }))
}

#[test]
fn test_rpc() {
    let mut nodes = build_nodes(2, None);
    // Random rpc message
    let rpc_request = RPCRequest::Hello(HelloMessage {
        fork_version: [0; 4],
        finalized_root: Hash256::from_low_u64_be(0),
        finalized_epoch: Epoch::new(1),
        head_root: Hash256::from_low_u64_be(0),
        head_slot: Slot::new(1),
    });
    tokio::run(futures::future::poll_fn(move || -> Result<_, ()> {
        for node in nodes.iter_mut() {
            loop {
                match node.poll().unwrap() {
                    Async::Ready(Some(Libp2pEvent::PeerDialed(peer_id))) => {
                        // Send an rpc message
                        node.swarm
                            .send_rpc(peer_id, RPCEvent::Request(1, rpc_request.clone()));
                    }
                    Async::Ready(Some(Libp2pEvent::RPC(_, event))) => match event {
                        // Should receive sent rpc message
                        RPCEvent::Request(id, request) => {
                            assert_eq!(id, 1);
                            assert_eq!(rpc_request.clone(), request);
                            return Ok(Async::Ready(()));
                        }
                        _ => panic!("Received incorrect rpc message"),
                    },
                    Async::Ready(Some(_)) => (),
                    Async::Ready(None) | Async::NotReady => break,
                }
            }
        }
        Ok(Async::NotReady)
    }))
}
