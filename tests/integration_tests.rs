#![cfg(itest)]

mod common;
use futures::future::try_join_all;
use lightning::bitcoin::constants::ChainHash;
use lightning::blinded_path::message::{MessageContext, OffersContext};
use lightning::blinded_path::IntroductionNode;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::nonce::Nonce;
use lightning::offers::offer::{Amount, Offer};
use lightning::util::string::PrintableString;
use log::error;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use bitcoincore_rpc::bitcoin::Network as RpcNetwork;
use ldk_sample::node_api::Node as LdkNode;
use lightning::offers::offer::Quantity;
use lightning::onion_message::messenger::Destination;
use lndk::lnd::validate_lnd_creds;
use lndk::offers::create_reply_path;
use lndk::offers::handler::{CreateOfferParams, OfferHandler, PayOfferParams};
use lndk::onion_messenger::MessengerUtilities;
use lndk::{setup_logger, LifecycleSignals};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::time;
use tokio::time::{sleep, timeout, Duration};
use tokio::{select, try_join};
use tonic_lnd::Client;

const NONCE_BYTES: &[u8] = &[42u8; 16];

// Helper function to get an invoice with a retry to ensure messenger initialization.
// This is necessary because there is a race condition between the messenger initialization,
// the internal lndk graph update, and calling get_invoice.
// Before, get_invoice sometimes would fail because internally the graph has not been updated, not
// finding the introduction node and failing when building the onion message.
async fn get_invoice_with_retry(
    handler: &Arc<OfferHandler>,
    offer: &lightning::offers::offer::Offer,
    amount: u64,
    network: Network,
    client: Client,
    destination: Destination,
) -> Result<(lightning::offers::invoice::Bolt12Invoice, u64, PaymentId), lndk::offers::OfferError> {
    let mut retries = 0;
    let max_retries = 3;
    let delay = Duration::from_secs(2);

    while retries < max_retries {
        tokio::time::sleep(delay).await;

        let result = handler
            .get_invoice(PayOfferParams {
                offer: offer.clone(),
                amount: Some(amount),
                payer_note: Some("".to_string()),
                network,
                client: client.clone(),
                destination: destination.clone(),
                reply_path: None,
                response_invoice_timeout: Some(15),
                fee_limit: None,
            })
            .await
            .map_err(|_| lndk::offers::OfferError::InvoiceTimeout(15));
        if result.is_ok() {
            return result;
        }
        retries += 1;
    }
    error!("Failed to get invoice after {} retries", max_retries);
    Err(lndk::offers::OfferError::InvoiceTimeout(15))
}

// Creates N offers and spits out the PayOfferParams that we can use to pay.
async fn create_offers(
    num: i32,
    ldk: &LdkNode,
    path_pubkeys: &Vec<PublicKey>,
    client: Client,
    _reply_path_keys: &Vec<PublicKey>,
) -> Vec<PayOfferParams> {
    let mut pay_cfgs = vec![];
    for _ in 0..num {
        let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
        let offer = ldk
            .create_offer(
                path_pubkeys,
                Network::Regtest,
                20_000,
                Quantity::One,
                expiration,
            )
            .await
            .expect("should create offer");

        let blinded_path = offer.paths()[0].clone();
        let pay_cfg = PayOfferParams {
            offer: offer,
            amount: Some(20_000),
            payer_note: Some("".to_string()),
            network: Network::Regtest,
            client: client.clone(),
            destination: Destination::BlindedPath(blinded_path),
            reply_path: None,
            response_invoice_timeout: None,
            fee_limit: None,
        };

        pay_cfgs.push(pay_cfg);
    }

    return pay_cfgs;
}

// A future that pays the same offer three times concurrently.
async fn pay_same_offer(handler: Arc<OfferHandler>, pay_cfg: PayOfferParams) -> Result<(), ()> {
    let fut1 = handler.pay_offer(pay_cfg.clone());
    let fut2 = handler.pay_offer(pay_cfg.clone());
    let fut3 = handler.pay_offer(pay_cfg);

    try_join!(fut1, fut2, fut3).map(|_| ()).map_err(|_| ())
}

// A future that pays different offers concurrently.
async fn pay_offers(handler: Arc<OfferHandler>, pay_cfgs: &Vec<PayOfferParams>) -> Result<(), ()> {
    let mut futs: Vec<_> = vec![];
    for i in 0..pay_cfgs.len() {
        futs.push(handler.pay_offer(pay_cfgs[i].clone()));
    }

    try_join_all(futs).await.map(|_| ()).map_err(|_| ())
}

#[tokio::test(flavor = "multi_thread")]
// Here we test the beginning of the BOLT 12 offers flow. We show that lndk successfully builds an
// invoice_request, sends it, and receives an invoice back.
async fn test_lndk_get_invoice() {
    let test_name = "lndk_get_invoice";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;
    let log_file = Some(lndk_dir.join(format!("lndk-logs.txt")));
    setup_logger(None, log_file).unwrap();
    // Here we'll produce a little network. ldk1 will be the offer creator in this scenario. We'll
    // connect ldk1 and ldk2 with a channel so ldk1 can create an offer and ldk2 can be the
    // introduction node for the blinded path.
    //
    // Later on we'll disconnect lnd to ldk2 to make sure lnd can still auto-connect to the
    // introduction node.
    //
    // ldk1 <--- channel ---> ldk2 <--- peer connection ---> lnd
    //
    // ldk1 will be the offer creator, which will build a blinded route from ldk2 to ldk1.
    let (pubkey, addr) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();

    ldk1.connect_to_peer(pubkey_2, addr_2).await.unwrap();
    lnd.connect_to_peer(pubkey_2, addr_2).await;

    let ldk2_fund_addr = ldk2.bitcoind_client.get_new_address().await;

    // We need to convert funding addresses to the form that the bitcoincore_rpc library recognizes.
    let ldk2_addr_string = ldk2_fund_addr.to_string();
    let ldk2_addr = bitcoincore_rpc::bitcoin::Address::from_str(&ldk2_addr_string)
        .unwrap()
        .require_network(RpcNetwork::Regtest)
        .unwrap();

    // Fund both of these nodes, open the channels, and synchronize the network.
    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk2_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    ldk2.open_channel(pubkey, addr, 200000, 0, true)
        .await
        .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(20, &ldk2_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    let path_pubkeys = vec![pubkey_2, pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    // Now we'll spin up lndk, which should forward the invoice request to ldk2.
    let (shutdown, listener) = triggered::trigger();

    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(&lnd.cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(&lnd.macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(lnd.address.clone(), creds);

    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg.clone(),
        signals,
        skip_version_check: false,
        rate_limit_count: 10,
        rate_limit_period_secs: 1,
    };

    let mut client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    log::debug!("waiting for ldk2's graph update to update lnd graph");

    let mut stream = client
        .lightning()
        .subscribe_channel_graph(tonic_lnd::lnrpc::GraphTopologySubscription {})
        .await
        .unwrap()
        .into_inner();

    // Wait for ldk2's graph update to come through, otherwise when we try to auto-connect to
    // the introduction node later on, the address won't be available when we call the
    // describe_graph API method.
    'outer: while let Some(update) = stream.message().await.unwrap() {
        for node in update.node_updates.iter() {
            for node_addr in node.node_addresses.iter() {
                if node_addr.addr == addr_2.to_string() {
                    break 'outer;
                }
            }
        }
    }

    let log_file = Some(lndk_dir.join(format!("lndk-logs.txt")));
    setup_logger(None, log_file).unwrap();

    // Make sure lndk successfully sends the invoice_request.
    let handler = Arc::new(OfferHandler::default());
    let messenger = lndk::LndkOnionMessenger::new();

    let destination = Destination::BlindedPath(blinded_path.clone());
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = get_invoice_with_retry(
            &handler,
            &offer,
            20_000,
            Network::Regtest,
            client.clone(),
            destination.clone(),
        ) => {
            assert!(res.is_ok());
        }
    }

    // Let's try again, but, make sure we can request the invoice when the LND node is not already
    // connected to the introduction node (LDK2).
    lnd.disconnect_peer(pubkey_2).await;
    lnd.wait_for_chain_sync().await;

    let (shutdown, listener) = triggered::trigger();
    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        signals,
        skip_version_check: false,
        rate_limit_count: 10,
        rate_limit_period_secs: 1,
    };

    let log_file = Some(lndk_dir.join(format!("lndk-logs.txt")));
    setup_logger(None, log_file).unwrap();

    let handler = Arc::new(OfferHandler::default());
    let messenger = lndk::LndkOnionMessenger::new();

    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = get_invoice_with_retry(
            &handler,
            &offer,
            20_000,
            Network::Regtest,
            client.clone(),
            destination.clone(),
        ) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to fully pay an offer.
async fn test_lndk_pay_offer() {
    let test_name = "lndk_pay_offer";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let (lndk_cfg, handler, messenger, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    let pay_cfg = PayOfferParams {
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: None,
        response_invoice_timeout: None,
        fee_limit: None,
    };
    select! {
        val = messenger.run(lndk_cfg.clone(), Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = handler.pay_offer(pay_cfg.clone()) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to pay the same offer multiple times concurrently.
async fn test_lndk_pay_offer_concurrently() {
    let test_name = "lndk_pay_offer_concurrently";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let (lndk_cfg, handler, messenger, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    let pay_cfg = PayOfferParams {
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: None,
        response_invoice_timeout: None,
        fee_limit: None,
    };
    // Let's also try to pay the same offer multiple times concurrently.
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = pay_same_offer(Arc::clone(&handler), pay_cfg) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to pay multiple offers at the same time.
async fn test_lndk_pay_multiple_offers_concurrently() {
    let test_name = "lndk_pay_multiple_offers_concurrently";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = &vec![ldk2_pubkey, ldk1_pubkey];
    let reply_path = &vec![ldk2_pubkey, lnd_pubkey];
    let pay_offer_cfgs = create_offers(
        3,
        &ldk1,
        path_pubkeys,
        lnd.client.clone().unwrap(),
        reply_path,
    )
    .await;

    let (lndk_cfg, handler, messenger, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    // Let's also try to pay multiple offers at the same time.
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = pay_offers(Arc::clone(&handler), &pay_offer_cfgs) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// We test that when creating a reply path for an offer node to send an invoice to, we don't
// use a node that we're connected to as the introduction node if it's an unadvertised node that
// is only connected by private channels.
async fn test_reply_path_unannounced_peers() {
    let test_name = "unannounced_peers";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (_, _, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, false, false, &mut lnd, &bitcoind).await;

    let (_, _, _, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let offer_context = OffersContext::OutboundPayment {
        payment_id: PaymentId([42; 32]),
        nonce: Nonce::try_from(NONCE_BYTES).unwrap(),
        hmac: None,
    };
    let offer_context = MessageContext::Offers(offer_context);
    let messenger_utils = MessengerUtilities::new([42; 32]);
    // In the small network we produced above, the lnd node is only connected to ldk2, which has a
    // private channel and as such, is an unadvertised node. Because of that, create_reply_path
    // should not use ldk2 as an introduction node and should return a reply path directly to
    // itself.
    let reply_path = create_reply_path(
        lnd.client.clone().unwrap().lightning().clone(),
        lnd_pubkey,
        offer_context,
        &messenger_utils,
    )
    .await;
    assert!(reply_path.is_ok());
    let reply_path = reply_path.unwrap();
    assert_eq!(reply_path.blinded_hops().len(), 1);

    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
// We test that when creating a reply path for an offer node to send an invoice to, we successfully
// use a node that we're connected to as the introduction node *if* it's an advertised node with
// public channels.
async fn test_reply_path_announced_peers() {
    let test_name = "announced_peers";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (_, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let (_, _, _, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let offer_context = OffersContext::OutboundPayment {
        payment_id: PaymentId([42; 32]),
        nonce: Nonce::try_from(NONCE_BYTES).unwrap(),
        hmac: None,
    };
    let offer_context = MessageContext::Offers(offer_context);
    let messenger_utils = MessengerUtilities::new([42; 32]);

    // In the small network we produced above, the lnd node is only connected to ldk2, which has a
    // public channel and as such, is indeed an advertised node. Because of this, we make sure
    // create_reply_path produces a path of length two with ldk2 as the introduction node, as we
    // expected.
    let reply_path = create_reply_path(
        lnd.client.clone().unwrap().lightning().clone(),
        lnd_pubkey,
        offer_context,
        &messenger_utils,
    )
    .await;
    assert!(reply_path.is_ok());
    let reply_path = reply_path.unwrap();
    assert_eq!(reply_path.blinded_hops().len(), 2);
    assert_eq!(
        *reply_path.introduction_node(),
        lightning::blinded_path::IntroductionNode::NodeId(ldk2_pubkey)
    );

    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to fully pay an offer.
async fn test_check_lndk_pay_offer_with_reconnection() {
    let test_name = "lndk_pay_offer_with_reconnection";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let (lndk_cfg, handler, messenger, shutdown) = common::setup_lndk(
        &lnd.cert_path,
        &lnd.macaroon_path,
        lnd.address.clone(),
        lndk_dir,
    )
    .await;

    let client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    let pay_cfg = PayOfferParams {
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: None,
        response_invoice_timeout: None,
        fee_limit: None,
    };
    select! {
        val = messenger.run(lndk_cfg.clone(), Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        _ = check_pay_offer_with_reconnection(handler, pay_cfg.clone(), lnd, ldk2_pubkey) => {
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    };
}

pub async fn test_payment_with_first_route_down(
    handler: &Arc<OfferHandler>,
    pay_cfg: &PayOfferParams,
    ldk3: &LdkNode,
    ldk4: &LdkNode,
    bitcoind: &common::BitcoindNode,
) -> Result<(), Box<dyn std::error::Error>> {
    // We first get the invoice and make sure we have 2 paths.
    let (invoice, amount, payment_id) = handler
        .get_invoice(pay_cfg.clone())
        .await
        .map_err(|e| format!("Failed to get invoice: {:?}", e))?;

    assert!(invoice.payment_paths().len() == 2);

    let first_path = invoice
        .payment_paths()
        .first()
        .ok_or("No payment paths found in invoice")?;

    let introduction_node = match first_path.introduction_node() {
        IntroductionNode::NodeId(pubkey) => *pubkey,
        IntroductionNode::DirectedShortChannelId(_, _) => {
            return Err("Introduction node is a short channel ID, not a public key".into());
        }
    };

    let node_to_isolate = if introduction_node == ldk3.get_node_info().0 {
        ldk3
    } else if introduction_node == ldk4.get_node_info().0 {
        ldk4
    } else {
        return Err("Introduction node is not a valid node".into());
    };

    let channels_info = node_to_isolate.list_channels().await;
    let address = bitcoind.node.client.new_address().unwrap();

    log::info!("Closing channels...");

    for channel in channels_info {
        node_to_isolate
            .close_channel(channel.0, channel.1)
            .await
            .unwrap();

        // We need to generate a block so we avoid that transaction output is unspendable.
        bitcoind
            .node
            .client
            .generate_to_address(1, &address)
            .unwrap();
    }

    log::info!("Waiting for list channels to be empty...");

    match timeout(Duration::from_secs(100), async {
        loop {
            let channels_info = node_to_isolate.list_channels().await;
            if channels_info.len() == 0 {
                break;
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    {
        Err(_) => panic!("timeout before channel closed"),
        _ => {}
    };
    log::info!("Attempting to pay invoice with first route down...");

    // Finally, we try to pay the invoice and make sure it succeeds.
    let payment_result = handler
        .pay_invoice(pay_cfg.client.clone(), amount, &invoice, payment_id, None)
        .await;

    match payment_result {
        Ok(_) => {
            log::info!("Payment succeeded despite first route being down!");
            Ok(())
        }
        Err(e) => {
            log::error!("Payment failed: {:?}", e);
            Err(format!("Payment failed: {:?}", e).into())
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// We test the case that one of the paths fails for some network reason and we
// retry the payment.
async fn test_lndk_pay_offer_with_retry() {
    let test_name = "lndk_pay_offer_with_retry";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, ldk_test_dir) =
        common::setup_test_infrastructure(test_name).await;

    let log_file = Some(lndk_dir.join(format!("lndk-logs.txt")));
    setup_logger(None, log_file).unwrap();
    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    // Second blinded path.
    let ldk3 = common::setup_ldk_node(&bitcoind, 3, &ldk_test_dir, test_name).await;
    // Blinded paths on ldk requires minimum 3 announced channels for a peer to be used.
    // So we add a new node to make sure we have 3 announced channels.
    //
    // Network topology with 4 LDK nodes and 1 LND node:
    //
    //         LDK2 ───unannounced───── LND
    //         /                      /|\
    //        /                      / | \
    //    LDK1 ──── LDK3 ────────────  |  \
    //      \       /                  |   \
    //       \     /                   |    \
    //        LDK4 ──────────────────────────
    //
    // This creates multiple paths for blinded payments with redundancy.
    // When one path fails, the payment can retry through alternate paths.
    // Note that LDK3 and LDK4 has 3 announced channels, so invoice will be created
    // with 2 blinded paths.
    // We are going to isolate either LDK3 or LDK4 closing their channels to LDK1.
    // Then, we will try to pay the invoice and make sure it succeeds.
    let ldk4 = common::setup_ldk_node(&bitcoind, 4, &ldk_test_dir, test_name).await;
    let (ldk3_pubkey, addr_3) = ldk3.get_node_info();
    let (ldk4_pubkey, addr_4) = ldk4.get_node_info();
    let (_, addr_1) = ldk1.get_node_info();
    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    ldk1.connect_to_peer(ldk4_pubkey, addr_4).await.unwrap();
    ldk1.connect_to_peer(ldk3_pubkey, addr_3).await.unwrap();
    lnd.connect_to_peer(ldk3_pubkey, addr_3).await;
    lnd.connect_to_peer(ldk4_pubkey, addr_4).await;

    let ldk3_fund_addr = ldk3.bitcoind_client.get_new_address().await;
    let ldk3_addr_string = ldk3_fund_addr.to_string();
    let ldk3_addr = bitcoincore_rpc::bitcoin::Address::from_str(&ldk3_addr_string)
        .unwrap()
        .require_network(bitcoincore_rpc::bitcoin::Network::Regtest)
        .unwrap();

    let ldk4_fund_addr = ldk4.bitcoind_client.get_new_address().await;
    let ldk4_addr_string = ldk4_fund_addr.to_string();
    let ldk4_addr = bitcoincore_rpc::bitcoin::Address::from_str(&ldk4_addr_string)
        .unwrap()
        .require_network(bitcoincore_rpc::bitcoin::Network::Regtest)
        .unwrap();

    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk3_addr)
        .unwrap();

    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk4_addr)
        .unwrap();
    lnd.wait_for_chain_sync().await;

    ldk3.open_channel(ldk1_pubkey, addr_1, 200000, 10000000, true)
        .await
        .unwrap();

    ldk4.open_channel(ldk1_pubkey, addr_1, 200000, 10000000, true)
        .await
        .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk3_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    let lnd_addr = lnd
        .address
        .replace("localhost", "127.0.0.1")
        .replace("https://", "");

    ldk3.open_channel(
        lnd_pubkey,
        SocketAddr::from_str(&lnd_addr).unwrap(),
        200000,
        10000000,
        true,
    )
    .await
    .unwrap();

    ldk4.open_channel(
        lnd_pubkey,
        SocketAddr::from_str(&lnd_addr).unwrap(),
        200000,
        10000000,
        true,
    )
    .await
    .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk3_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    ldk3.open_channel(ldk4_pubkey, addr_4, 200000, 10000000, true)
        .await
        .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(20, &ldk3_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;
    lnd.wait_for_nodes_addresses(&[&ldk1, &ldk2, &ldk3, &ldk4])
        .await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let (lndk_cfg, handler, messenger, shutdown) = common::setup_lndk(
        &lnd.cert_path,
        &lnd.macaroon_path,
        lnd.address.clone(),
        lndk_dir,
    )
    .await;

    let client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    let pay_cfg = PayOfferParams {
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: None,
        response_invoice_timeout: None,
        fee_limit: None,
    };

    select! {
        val = messenger.run(lndk_cfg.clone(), Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = test_payment_with_first_route_down(&handler, &pay_cfg, &ldk3, &ldk4, &bitcoind) => {
            log::info!("res: {:?}", res);
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    };
}

async fn check_pay_offer_with_reconnection(
    handler: Arc<OfferHandler>,
    pay_cfg: PayOfferParams,
    lnd: common::LndNode,
    node_id: PublicKey,
) {
    let lnd_arc = Arc::new(tokio::sync::Mutex::new(lnd));
    let lnd_clone = Arc::clone(&lnd_arc);

    // Setup a task to kill LND after the pay_offer has already started.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(270)).await;
        let mut lnd = lnd_clone.lock().await;
        lnd.kill_lnd().await;
    });

    // Start a pay_offer process.
    let res1 = handler.pay_offer(pay_cfg.clone()).await;
    let mut lnd = lnd_arc.lock().await;

    // Restart LND node with the same previous arguments.
    lnd.restart_lnd().await;

    let interval = time::interval(Duration::from_millis(500));

    // Wait until LND is available again
    lnd.check_lnd_running(interval).await.unwrap();

    // Wait until lnd graph some addresses for node_id.
    lnd.wait_for_addresses_to_sync(node_id).await;

    // Send another pay_offer process using the same handler.
    // Because of the reconnections, the handler has to be able to connect
    // again to the restart LND node, fetched the peers, and be able to handle
    // a second pay_offer
    let res2 = handler.pay_offer(pay_cfg.clone()).await;

    // We check that the first pay_offer fails because the LND has been kill.
    assert!(res1.is_err());

    // We check that the second pay_offer success using the same handler
    // because the LND node has been restarted.
    assert!(res2.is_ok());
}
#[tokio::test(flavor = "multi_thread")]
// Test that we can create an offer and that the offer is valid.
async fn test_create_offer() {
    let test_name = "lndk_create_offer";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (_, _ldk2_pubkey, _lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, false, true, &mut lnd, &bitcoind).await;

    let (_, handler, _, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let create_offer_params = CreateOfferParams {
        client: lnd.client.clone().unwrap(),
        amount_msats: 20_000,
        chain: Network::Regtest,
        description: None,
        issuer: None,
        quantity: None,
        expiry: None,
    };
    let offer = handler.create_offer(create_offer_params).await;
    assert!(offer.is_ok());
    let offer = offer.unwrap();
    assert_eq!(
        offer.amount().unwrap(),
        Amount::Bitcoin {
            amount_msats: 20_000
        }
    );
    assert_eq!(offer.chains(), vec![ChainHash::REGTEST]);
    assert_eq!(offer.description(), Some(PrintableString("")));
    assert_eq!(offer.issuer(), None);
    assert_eq!(offer.supported_quantity(), Quantity::One);
    assert_eq!(offer.absolute_expiry(), None);
    log::info!("Sending InvoiceRequest ldk2 --> lnd");
    let payment = ldk2.pay_offer(offer, None).await;
    log::info!("Payment sent: {:?}", payment);
    assert!(payment.is_ok());
    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}

async fn pay_offer_and_wait_for_payment(
    ldk: &LdkNode,
    offer: Offer,
    mut lnd_client: Client,
) -> Result<(), ()> {
    let payment = ldk.pay_offer(offer, None).await;
    assert!(payment.is_ok());
    // Wait for the payment to complete on ldk side.
    common::wait_for_ldk_payment_completion(ldk, Duration::from_secs(30)).await?;
    // Wait for the payment to complete on lnd side.
    common::wait_for_lnd_payment_completion(&mut lnd_client, Duration::from_secs(10)).await?;
    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
// Test that we can receive a payment from an offer.
async fn test_receive_payment_from_offer() {
    let test_name = "lndk_receive_payment_from_offer";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir, _) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, _ldk2_pubkey, _lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, true, true, &mut lnd, &bitcoind).await;

    let log_file = Some(lndk_dir.join(format!("lndk-logs.txt")));
    setup_logger(None, log_file).unwrap();

    let (shutdown, listener) = triggered::trigger();
    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(&lnd.cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(&lnd.macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(lnd.address.clone(), creds);

    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        signals,
        skip_version_check: false,
        rate_limit_count: 10,
        rate_limit_period_secs: 1,
    };
    let handler = Arc::new(OfferHandler::new(
        None,
        None,
        Some(lnd.client.clone().unwrap()),
    ));
    let messenger = lndk::LndkOnionMessenger::new();

    let create_offer_params = CreateOfferParams {
        client: lnd.client.clone().unwrap(),
        amount_msats: 20_000,
        chain: Network::Regtest,
        description: None,
        issuer: None,
        quantity: None,
        expiry: None,
    };

    let offer = handler.create_offer(create_offer_params).await;
    assert!(offer.is_ok());
    let offer = offer.unwrap();
    lnd.wait_for_addresses_to_sync(ldk1_pubkey).await;

    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = pay_offer_and_wait_for_payment(&ldk1, offer, lnd.client.clone().unwrap()) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    };
}
