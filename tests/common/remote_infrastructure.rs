use tonic_lnd::Client;

use super::test_utils;
pub struct RemoteLndNode {
    pub address: String,
    pub cert_path: String,
    pub macaroon_path: String,
    pub client: Option<Client>,
}

impl RemoteLndNode {
    pub fn new(address: String, cert_path: String, macaroon_path: String) -> Self {
        Self {
            address,
            cert_path,
            macaroon_path,
            client: None,
        }
    }

    pub async fn setup_client(&mut self) {
        response = test_utils::retry_async(
            || async {
                tonic_lnd::connect(
                    self.address.clone(),
                    self.cert_path.clone(),
                    self.macaroon_path.clone(),
                )
                .await
            },
            "setup_remote_client".to_string(),
        );

        match response {
            Ok(client) => self.client = Some(client),
            Err(e) => panic!("Failed to setup remote client: {}", e),
        }
    }
}
