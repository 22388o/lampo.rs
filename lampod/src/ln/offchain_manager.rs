//! Lampo Offchain manager.
//!
//! The offchain manager will manage all the necessary
//! information about the lightning network operation.
//!
//! Such as generate and invoice or pay an invoice.
//!
//! This module will also be able to interact with
//! other feature like onion message, and more general
//! with the network graph. But this is not so clear yet.
//!
//! Author: Vincenzo Palazzo <vincenzopalazzo@member.fsf.org>
use std::sync::Arc;
use std::time::Duration;

use lampo_common::bitcoin::hashes::sha256::Hash as Sha256;
use lampo_common::bitcoin::hashes::Hash;
use lampo_common::bitcoin::secp256k1::PublicKey as pubkey;
use lampo_common::conf::LampoConf;
use lampo_common::error;
use lampo_common::keymanager::KeysManager;
use lampo_common::ldk;
use lampo_common::ldk::ln::channelmanager::Retry;
use lampo_common::ldk::ln::channelmanager::{PaymentId, RecipientOnionFields};
use lampo_common::ldk::ln::{PaymentHash, PaymentPreimage};
use lampo_common::ldk::routing::router::{PaymentParameters, RouteParameters};
use lampo_common::ldk::sign::EntropySource;

use super::LampoChannelManager;
use crate::chain::LampoChainManager;
use crate::utils::logger::LampoLogger;

pub struct OffchainManager {
    channel_manager: Arc<LampoChannelManager>,
    keys_manager: Arc<KeysManager>,
    logger: Arc<LampoLogger>,
    lampo_conf: Arc<LampoConf>,
    chain_manager: Arc<LampoChainManager>,
}

impl OffchainManager {
    // FIXME: use the build pattern here
    pub fn new(
        keys_manager: Arc<KeysManager>,
        channel_manager: Arc<LampoChannelManager>,
        logger: Arc<LampoLogger>,
        lampo_conf: Arc<LampoConf>,
        chain_manager: Arc<LampoChainManager>,
    ) -> error::Result<Self> {
        Ok(Self {
            channel_manager,
            keys_manager,
            logger,
            lampo_conf,
            chain_manager,
        })
    }

    /// Generate an invoice with a specific amount and a specific
    /// description.
    pub fn generate_invoice(
        &self,
        amount_msat: Option<u64>,
        description: &str,
        expiring_in: u32,
    ) -> error::Result<ldk::invoice::Bolt11Invoice> {
        let currency = ldk::invoice::Currency::try_from(self.lampo_conf.network)?;
        let invoice = ldk::invoice::utils::create_invoice_from_channelmanager(
            &self.channel_manager.manager(),
            self.keys_manager.clone(),
            self.logger.clone(),
            currency,
            amount_msat,
            description.to_string(),
            expiring_in,
            None,
            // FIXME: improve the error inside the ldk side
        )
        .map_err(|err| error::anyhow!(err))?;
        Ok(invoice)
    }

    pub fn decode_invoice(&self, invoice_str: &str) -> error::Result<ldk::invoice::Bolt11Invoice> {
        let invoice = invoice_str.parse::<ldk::invoice::Bolt11Invoice>()?;
        Ok(invoice)
    }

    pub fn pay_invoice(&self, invoice_str: &str, amount_msat: Option<u64>) -> error::Result<()> {
        let invoice = self.decode_invoice(invoice_str)?;
        let channel_manager = self.channel_manager.manager();
        let channel_manager = channel_manager.as_ref();
        if invoice.amount_milli_satoshis().is_none() {
            ldk::invoice::payment::pay_zero_value_invoice(
                &invoice,
                amount_msat.ok_or(error::anyhow!(
                    "invoice with no amount, and amount must be specified"
                ))?,
                Retry::Timeout(Duration::from_secs(10)),
                channel_manager,
            )
            .map_err(|err| error::anyhow!("{:?}", err))?;
        } else {
            ldk::invoice::payment::pay_invoice(
                &invoice,
                Retry::Timeout(Duration::from_secs(10)),
                channel_manager,
            )
            .map_err(|err| error::anyhow!("{:?}", err))?;
        }
        Ok(())
    }
    pub fn keysend(&self, destination: pubkey, amount_msat: u64) -> error::Result<PaymentHash> {
        let payment_preimage = PaymentPreimage(
            self.chain_manager
                .wallet_manager
                .ldk_keys()
                .keys_manager
                .clone()
                .get_secure_random_bytes(),
        );
        let PaymentPreimage(bytes) = payment_preimage;
        let payment_hash = PaymentHash(Sha256::hash(&bytes).into_inner());
        // The 40 here is the max CheckLockTimeVerify which locks the output of the transaction for a certain
        // period of time.The false here stands for the allow_mpp, which is to allow the multi part route payments.
        let route_params = RouteParameters {
            payment_params: PaymentParameters::for_keysend(destination, 40, false),
            final_value_msat: amount_msat,
            max_total_routing_fee_msat: None,
        };
        log::info!("Initialised Keysend");
        let payment_result = self
            .channel_manager
            .manager()
            .send_spontaneous_payment_with_retry(
                Some(payment_preimage),
                RecipientOnionFields::spontaneous_empty(),
                PaymentId(payment_hash.0),
                route_params,
                Retry::Timeout(Duration::from_secs(10)),
            )
            .map_err(|err| error::anyhow!("{:?}", err))?;
        log::info!("Keysend successfully done!");
        Ok(payment_result)
    }
}
