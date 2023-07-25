//! Wallet Manager implementation with BDK
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use bdk::bitcoin::Amount;
use bdk::keys::bip39::{Language, Mnemonic, WordCount};
use bdk::keys::GeneratableKey;
use bdk::keys::{DerivableKey, ExtendedKey, GeneratedKey};
use bdk::template::Bip84;
use bdk::wallet::ChangeSet;
use bdk::{FeeRate, KeychainKind, SignOptions, Wallet};
use bdk_esplora::EsploraExt;
use bdk_file_store::Store;
use log;

use lampo_common::bitcoin::hashes::hex::ToHex;
use lampo_common::bitcoin::util::bip32::ExtendedPrivKey;
use lampo_common::bitcoin::{PrivateKey, Script, Transaction};
use lampo_common::conf::{LampoConf, Network};
use lampo_common::error;
use lampo_common::keys::LampoKeys;
use lampo_common::model::response::{NewAddress, Utxo};
use lampo_common::wallet::WalletManager;

pub struct BDKWalletManager {
    pub wallet: RefCell<Mutex<Wallet<Store<'static, ChangeSet>>>>,
    pub keymanager: Arc<LampoKeys>,
    pub network: Network,
}

// SAFETY: It is safe to do because the `LampoWalletManager`
// is not send and sync due the RefCell, but we use the Mutex
// inside, so we are safe to share across threads.
unsafe impl Send for BDKWalletManager {}
unsafe impl Sync for BDKWalletManager {}

impl BDKWalletManager {
    /// from mnemonic_words build or bkd::Wallet or return an bdk::Error
    fn build_wallet(
        conf: Arc<LampoConf>,
        mnemonic_words: &str,
    ) -> Result<(Wallet<Store<'static, ChangeSet>>, LampoKeys), bdk::Error> {
        // Parse a mnemonic
        let mnemonic =
            Mnemonic::parse(mnemonic_words).map_err(|err| bdk::Error::Generic(format!("{err}")))?;
        // Generate the extended key
        let xkey: ExtendedKey = mnemonic.into_extended_key()?;
        // Get xprv from the extended key
        let xprv = xkey.into_xprv(conf.network).ok_or(bdk::Error::Generic(
            "wrong convertion to a private key".to_string(),
        ))?;

        let db = Store::<ChangeSet>::new_from_path(
            "lampo".as_bytes(),
            format!("{}/onchain", conf.path()),
        )
        .map_err(|err| bdk::Error::Generic(format!("{err}")))?;
        let ldk_kesy = LampoKeys::new(xprv.private_key.secret_bytes());
        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")
        let wallet = Wallet::new(
            Bip84(xprv, KeychainKind::External),
            Some(Bip84(xprv, KeychainKind::Internal)),
            db,
            conf.network,
        )
        .map_err(|err| bdk::Error::Generic(err.to_string()))?;
        let descriptor = wallet.public_descriptor(KeychainKind::Internal).unwrap();
        log::info!("descriptor: {descriptor}");
        Ok((wallet, ldk_kesy))
    }

    #[cfg(debug_assertions)]
    fn build_from_private_key(
        xprv: PrivateKey,
        channel_keys: Option<String>,
    ) -> Result<(Wallet<Store<'static, ChangeSet>>, LampoKeys), bdk::Error> {
        let ldk_keys = if channel_keys.is_some() {
            LampoKeys::with_channel_keys(xprv.inner.secret_bytes(), channel_keys.unwrap())
        } else {
            LampoKeys::new(xprv.inner.secret_bytes())
        };

        // FIXME: Get a tmp path
        let db = Store::new_from_path("lampo".as_bytes(), "/tmp/onchain")
            .map_err(|err| bdk::Error::Generic(format!("{err}")))?;

        let key = ExtendedPrivKey::new_master(xprv.network, &xprv.inner.secret_bytes())?;
        let key = ExtendedKey::from(key);
        let wallet = Wallet::new(Bip84(key, KeychainKind::External), None, db, xprv.network)
            .map_err(|err| bdk::Error::Generic(err.to_string()))?;
        Ok((wallet, ldk_keys))
    }
}

impl WalletManager for BDKWalletManager {
    fn new(conf: Arc<LampoConf>) -> error::Result<(Self, String)> {
        // Generate fresh mnemonic
        let mnemonic: GeneratedKey<_, bdk::miniscript::Tap> =
            Mnemonic::generate((WordCount::Words12, Language::English))
                .map_err(|err| bdk::Error::Generic(format!("{:?}", err)))?;
        // Convert mnemonic to string
        let mnemonic_words = mnemonic.to_string();
        log::info!("mnemonic works `{mnemonic_words}`");
        let (wallet, keymanager) = BDKWalletManager::build_wallet(conf.clone(), &mnemonic_words)?;
        Ok((
            Self {
                wallet: RefCell::new(Mutex::new(wallet)),
                keymanager: Arc::new(keymanager),
                network: conf.network,
            },
            mnemonic_words,
        ))
    }

    fn restore(conf: Arc<LampoConf>, mnemonic_words: &str) -> error::Result<Self> {
        let (wallet, keymanager) = BDKWalletManager::build_wallet(conf.clone(), mnemonic_words)?;
        Ok(Self {
            wallet: RefCell::new(Mutex::new(wallet)),
            keymanager: Arc::new(keymanager),
            network: conf.network,
        })
    }

    fn ldk_keys(&self) -> Arc<LampoKeys> {
        self.keymanager.clone()
    }

    fn get_onchain_address(&self) -> error::Result<NewAddress> {
        let address = self
            .wallet
            .borrow_mut()
            .lock()
            .unwrap()
            .get_address(bdk::wallet::AddressIndex::New);
        Ok(NewAddress {
            address: address.address.to_string(),
        })
    }

    fn get_onchain_balance(&self) -> error::Result<u64> {
        self.sync()?;
        let balance = self.wallet.borrow().lock().unwrap().get_balance();
        Ok(balance.confirmed)
    }

    fn create_transaction(
        &self,
        script: Script,
        amount: u64,
        fee_rate: u32,
    ) -> error::Result<Transaction> {
        self.sync()?;
        let wallet = self.wallet.borrow_mut();
        let mut wallet = wallet.lock().unwrap();
        let mut tx = wallet.build_tx();
        tx.add_recipient(script, amount)
            .fee_rate(FeeRate::from_sat_per_kvb(fee_rate as f32))
            .enable_rbf();
        let (mut psbt, _) = tx.finish()?;
        if !wallet.sign(&mut psbt, SignOptions::default())? {
            error::bail!("wallet not able to sing the psbt {psbt}");
        }
        if !wallet.finalize_psbt(&mut psbt, SignOptions::default())? {
            error::bail!("wallet impossible finalize the psbt: {psbt}");
        };
        Ok(psbt.extract_tx())
    }

    fn list_transactions(&self) -> error::Result<Vec<Utxo>> {
        self.sync()?;
        let wallet = self.wallet.borrow();
        let wallet = wallet.lock().unwrap();
        let txs = wallet
            .list_unspent()
            .map(|tx| Utxo {
                txid: tx.outpoint.txid.to_hex(),
                vout: tx.outpoint.vout,
                reserved: tx.is_spent,
                confirmed: 0,
                amount_msat: Amount::from_btc(tx.txout.value as f64).unwrap().to_sat()
                    * 1000 as u64,
            })
            .collect::<Vec<_>>();
        Ok(txs)
    }

    fn sync(&self) -> error::Result<()> {
        // Scanning the chain...
        let esplora_url = match self.network {
            Network::Bitcoin => "https://mempool.space/api",
            Network::Testnet => "https://mempool.space/testnet/api",
            _ => {
                error::bail!("network `{:?}` not supported", self.network);
            }
        };
        let wallet = self.wallet.borrow();
        let mut wallet = wallet.lock().unwrap();
        let client = bdk_esplora::esplora_client::Builder::new(esplora_url).build_blocking()?;
        let checkpoints = wallet.checkpoints();
        let spks = wallet
            .spks_of_all_keychains()
            .into_iter()
            .map(|(k, spks)| {
                let mut first = true;
                (
                    k,
                    spks.inspect(move |(spk_i, _)| {
                        if first {
                            first = false;
                        }
                    }),
                )
            })
            .collect();
        log::info!("bdk stert to sync");
        let update = client.scan(
            checkpoints,
            spks,
            core::iter::empty(),
            core::iter::empty(),
            50,
            2,
        )?;
        wallet.apply_update(update)?;
        wallet.commit()?;
        log::info!(
            "bdk in sync at height {}!",
            client
                .get_height()
                .map_err(|err| bdk::Error::Generic(format!("{err}")))?
        );
        Ok(())
    }
}

#[cfg(debug_assertions)]
impl TryFrom<(PrivateKey, Option<String>)> for BDKWalletManager {
    type Error = bdk::Error;

    fn try_from(value: (PrivateKey, Option<String>)) -> Result<Self, Self::Error> {
        let (wallet, keymanager) = BDKWalletManager::build_from_private_key(value.0, value.1)?;
        Ok(Self {
            wallet: RefCell::new(Mutex::new(wallet)),
            keymanager: Arc::new(keymanager),
            // This should be possible only during integration testing
            // FIXME: fix the sync method in bdk, the esplora client will crash!
            network: Network::Regtest,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lampo_common::bitcoin;
    use lampo_common::bitcoin::PrivateKey;
    use lampo_common::secp256k1::SecretKey;

    use super::{BDKWalletManager, WalletManager};

    #[test]
    fn from_private_key() {
        let pkey = PrivateKey::new(
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
            bitcoin::Network::Regtest,
        );
        let wallet = BDKWalletManager::try_from((pkey, None));
        assert!(wallet.is_ok(), "{:?}", wallet.err());
        let wallet = wallet.unwrap();
        assert!(wallet.get_onchain_address().is_ok());
    }
}
