use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use tokio::sync::RwLock;
use tonic_lnd::tonic::Status;

use crate::lnd::PeerConnector;

/// Tracks the connection status we believe our LND node has with other nodes.
#[derive(Default)]
pub(crate) struct PeerState {
    peers: Arc<RwLock<HashSet<PublicKey>>>,
}

pub(crate) enum PeerStateError {
    /// The peer is not connected.
    PeerNotConnected,
    /// The `list_peers` call failed. We are not able to determine if the peer is connected.
    PeerConnectError(Status),
}

impl Display for PeerStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerStateError::PeerNotConnected => write!(f, "Peer not connected"),
            PeerStateError::PeerConnectError(e) => write!(
                f,
                "We are not able to determine if the peer is connected: {e:?}"
            ),
        }
    }
}

impl PeerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure we are connected to `node_id`, updating the cache as needed.
    /// Fast-path returns immediately if we *believe* we are connected.
    pub(crate) async fn ensure_connected<C: PeerConnector + Send>(
        &self,
        connector: &mut C,
        node_id: PublicKey,
    ) -> Result<(), PeerStateError> {
        {
            let connected_peers = self.peers.read().await;
            if connected_peers.contains(&node_id) {
                return Ok(());
            }
        }
        // Re-check with LND once (cheap if already connected)
        let peers = connector
            .list_peers()
            .await
            .map_err(PeerStateError::PeerConnectError)?;
        if peers.peers.iter().any(|p| p.pub_key == node_id.to_string()) {
            {
                let mut connected_peers = self.peers.write().await;
                connected_peers.insert(node_id);
            }
            return Ok(());
        }
        Err(PeerStateError::PeerNotConnected)
    }

    /// Call when we learn a peer got disconnected.
    pub(crate) async fn mark_disconnected(&self, node_id: &PublicKey) {
        {
            let mut connected_peers = self.peers.write().await;
            connected_peers.remove(node_id);
        }
    }

    /// Call when we learn a peer got connected.
    pub(crate) async fn mark_connected(&self, node_id: &PublicKey) {
        {
            let mut connected_peers = self.peers.write().await;
            connected_peers.insert(*node_id);
        }
    }

    pub(crate) async fn is_connected(&self, node_id: &PublicKey) -> bool {
        let connected_peers = self.peers.read().await;
        connected_peers.contains(node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lnd::PeerConnector;
    use bitcoin::secp256k1::PublicKey;
    use mockall::mock;
    use std::str::FromStr;
    use tonic::async_trait;
    use tonic_lnd::tonic::Status;

    mock! {
        TestPeerConnector{}

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<tonic_lnd::lnrpc::NodeInfo, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
         }
    }

    #[tokio::test]
    async fn test_ensure_connected_already_connected() {
        let peer_state = PeerState::new();
        let pubkey = PublicKey::from_str(
            "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba",
        )
        .unwrap();

        peer_state.mark_connected(&pubkey).await;

        let mut connector = MockTestPeerConnector::new();
        let result = peer_state.ensure_connected(&mut connector, pubkey).await;

        assert!(result.is_ok());
        assert!(peer_state.is_connected(&pubkey).await);
    }

    #[tokio::test]
    async fn test_ensure_connected_not_connected() {
        let peer_state = PeerState::new();
        let pubkey = PublicKey::from_str(
            "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba",
        )
        .unwrap();

        let mut connector = MockTestPeerConnector::new();

        connector.expect_list_peers().returning(move || {
            Ok(tonic_lnd::lnrpc::ListPeersResponse {
                peers: vec![tonic_lnd::lnrpc::Peer {
                    pub_key: pubkey.to_string(),
                    ..Default::default()
                }],
            })
        });
        let result = peer_state.ensure_connected(&mut connector, pubkey).await;

        assert!(result.is_ok());
        assert!(peer_state.is_connected(&pubkey).await);
    }

    #[tokio::test]
    async fn test_mark_disconnected() {
        let peer_state = PeerState::new();
        let pubkey = PublicKey::from_str(
            "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba",
        )
        .unwrap();

        peer_state.mark_connected(&pubkey).await;
        assert!(peer_state.is_connected(&pubkey).await);

        peer_state.mark_disconnected(&pubkey).await;
        assert!(!peer_state.is_connected(&pubkey).await);
    }
}
