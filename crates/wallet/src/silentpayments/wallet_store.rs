use std::{
    collections::HashMap,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use bitcoin::secp256k1::Scalar;
use bitcoin::{hashes::Hash, Amount, OutPoint, ScriptBuf, Txid};

use crate::io::FileExt;
use crate::silentpayments::{Coin, Network, SpentBy, Wallet};

// "S"ilent "P"ayments "W"allet "F"ile
const MAGIC: &[u8; 4] = b"SPWF";
const VERSION: u8 = 1;

const HEADER_LEN: usize = 10;
const COUNT_LEN: usize = 4;
const RECORD_LEN: usize = 192;
const SCRIPT_LEN: usize = 34;

const NETWORK_MAINNET: u8 = 0;
const NETWORK_TESTNET: u8 = 1;
const NETWORK_REGTEST: u8 = 2;

const SPENT_FLAG_UNSPENT: u8 = 0;
const SPENT_FLAG_SPENT: u8 = 1;

const LABEL_TAG_NONE: u8 = 0;
const LABEL_TAG_SOME: u8 = 1;

const R_SPENT_FLAG: usize = 0;
const R_TXID: usize = R_SPENT_FLAG + 1;
const R_VOUT: usize = R_TXID + 32;
const R_VALUE: usize = R_VOUT + 4;
const R_SCRIPT: usize = R_VALUE + 8;
const R_TWEAK: usize = R_SCRIPT + SCRIPT_LEN;
const R_LABEL_TAG: usize = R_TWEAK + 32;
const R_LABEL_SCALAR: usize = R_LABEL_TAG + 1;
const R_BLOCK_HEIGHT: usize = R_LABEL_SCALAR + 32;
const R_SPENT_TXID: usize = R_BLOCK_HEIGHT + 4;
const R_SPENT_HEIGHT: usize = R_SPENT_TXID + 32;

#[derive(Debug)]
pub enum WalletPersistenceError {
    Io(io::Error),
    BadMagic,
    WrongVersion(u8),
    UnknownNetwork(u8),
    UnknownSpentFlag(u8),
    UnknownLabelTag(u8),
    InvalidScalar,
    NonP2trScript { got: usize },
    TruncatedFile,
}

impl fmt::Display for WalletPersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o: {e}"),
            Self::BadMagic => write!(f, "bad magic bytes"),
            Self::WrongVersion(v) => write!(f, "unsupported version: {v}"),
            Self::UnknownNetwork(n) => write!(f, "unknown network byte: {n}"),
            Self::UnknownSpentFlag(s) => write!(f, "unknown spent flag byte: {s}"),
            Self::UnknownLabelTag(t) => write!(f, "unknown label tag byte: {t}"),
            Self::InvalidScalar => write!(f, "invalid scalar in record"),
            Self::NonP2trScript { got } => {
                write!(f, "script_pubkey is {got} bytes, expected {SCRIPT_LEN}")
            }
            Self::TruncatedFile => write!(f, "file is truncated"),
        }
    }
}

impl std::error::Error for WalletPersistenceError {}

impl From<io::Error> for WalletPersistenceError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug)]
pub enum WalletStoreError {
    Persistence(WalletPersistenceError),
    NetworkMismatch { file: Network, expected: Network },
}

impl fmt::Display for WalletStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistence(e) => write!(f, "{e}"),
            Self::NetworkMismatch { file, expected } => write!(
                f,
                "wallet file network ({file:?}) does not match configured network ({expected:?})"
            ),
        }
    }
}

impl std::error::Error for WalletStoreError {}

impl From<WalletPersistenceError> for WalletStoreError {
    fn from(e: WalletPersistenceError) -> Self {
        Self::Persistence(e)
    }
}

impl FileExt for Wallet {
    type Error = WalletPersistenceError;

    fn save(&self, path: &Path) -> Result<(), Self::Error> {
        let bytes = wallet_to_bytes(self)?;
        save_atomic(path, &bytes)
    }

    fn load(path: &Path) -> Result<Self, Self::Error> {
        let bytes = fs::read(path)?;
        wallet_from_bytes(&bytes)
    }
}

#[derive(Debug, Clone)]
pub struct WalletStore {
    path: PathBuf,
    network: Network,
}

impl WalletStore {
    pub fn new(path: PathBuf, network: Network) -> Self {
        Self { path, network }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn save(&self, wallet: &Wallet) -> Result<(), WalletPersistenceError> {
        wallet.save(&self.path)
    }

    pub fn load(&self) -> Result<Wallet, WalletStoreError> {
        let wallet = Wallet::load(&self.path)?;
        if wallet.network != self.network {
            return Err(WalletStoreError::NetworkMismatch {
                file: wallet.network,
                expected: self.network,
            });
        }
        Ok(wallet)
    }
}

fn wallet_to_bytes(wallet: &Wallet) -> Result<Vec<u8>, WalletPersistenceError> {
    let coin_count: u32 = wallet
        .utxos
        .len()
        .try_into()
        .expect("utxo count fits in u32");

    let mut buf = Vec::with_capacity(HEADER_LEN + COUNT_LEN + wallet.utxos.len() * RECORD_LEN);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(encode_network(wallet.network));
    buf.extend_from_slice(&wallet.scan_height.to_le_bytes());
    buf.extend_from_slice(&coin_count.to_le_bytes());

    for (outpoint, coin) in &wallet.utxos {
        buf.extend_from_slice(&encode_record(outpoint, coin)?);
    }
    Ok(buf)
}

fn wallet_from_bytes(bytes: &[u8]) -> Result<Wallet, WalletPersistenceError> {
    if bytes.len() < HEADER_LEN + COUNT_LEN {
        return Err(WalletPersistenceError::TruncatedFile);
    }
    if &bytes[0..4] != MAGIC {
        return Err(WalletPersistenceError::BadMagic);
    }
    if bytes[4] != VERSION {
        return Err(WalletPersistenceError::WrongVersion(bytes[4]));
    }
    let network = decode_network(bytes[5])?;
    let scan_height = u32::from_le_bytes(bytes[6..10].try_into().unwrap());
    let coin_count = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;

    let expected_len = HEADER_LEN + COUNT_LEN + coin_count * RECORD_LEN;
    if bytes.len() != expected_len {
        return Err(WalletPersistenceError::TruncatedFile);
    }

    let mut utxos = HashMap::with_capacity(coin_count);
    for i in 0..coin_count {
        let off = HEADER_LEN + COUNT_LEN + i * RECORD_LEN;
        let record: &[u8; RECORD_LEN] = bytes[off..off + RECORD_LEN].try_into().unwrap();
        let (outpoint, coin) = decode_record(record)?;
        utxos.insert(outpoint, coin);
    }

    Ok(Wallet::from_parts(scan_height, network, utxos))
}

fn save_atomic(path: &Path, bytes: &[u8]) -> Result<(), WalletPersistenceError> {
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp: PathBuf = tmp_os.into();

    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn encode_network(n: Network) -> u8 {
    match n {
        Network::Mainnet => NETWORK_MAINNET,
        Network::Testnet => NETWORK_TESTNET,
        Network::Regtest => NETWORK_REGTEST,
    }
}

fn decode_network(b: u8) -> Result<Network, WalletPersistenceError> {
    match b {
        NETWORK_MAINNET => Ok(Network::Mainnet),
        NETWORK_TESTNET => Ok(Network::Testnet),
        NETWORK_REGTEST => Ok(Network::Regtest),
        n => Err(WalletPersistenceError::UnknownNetwork(n)),
    }
}

fn encode_record(
    outpoint: &OutPoint,
    coin: &Coin,
) -> Result<[u8; RECORD_LEN], WalletPersistenceError> {
    let script = coin.script_pubkey.as_bytes();
    if script.len() != SCRIPT_LEN {
        return Err(WalletPersistenceError::NonP2trScript { got: script.len() });
    }

    let mut buf = [0u8; RECORD_LEN];
    buf[R_SPENT_FLAG] = if coin.spent_by.is_some() {
        SPENT_FLAG_SPENT
    } else {
        SPENT_FLAG_UNSPENT
    };
    buf[R_TXID..R_TXID + 32].copy_from_slice(&outpoint.txid.to_byte_array());
    buf[R_VOUT..R_VOUT + 4].copy_from_slice(&outpoint.vout.to_le_bytes());
    buf[R_VALUE..R_VALUE + 8].copy_from_slice(&coin.value.to_sat().to_le_bytes());
    buf[R_SCRIPT..R_SCRIPT + SCRIPT_LEN].copy_from_slice(script);
    buf[R_TWEAK..R_TWEAK + 32].copy_from_slice(&coin.tweak.to_be_bytes());

    if let Some(label) = coin.label {
        buf[R_LABEL_TAG] = LABEL_TAG_SOME;
        buf[R_LABEL_SCALAR..R_LABEL_SCALAR + 32].copy_from_slice(&label.to_be_bytes());
    } else {
        buf[R_LABEL_TAG] = LABEL_TAG_NONE;
    }

    buf[R_BLOCK_HEIGHT..R_BLOCK_HEIGHT + 4].copy_from_slice(&coin.block_height.to_le_bytes());

    if let Some(spent_by) = &coin.spent_by {
        buf[R_SPENT_TXID..R_SPENT_TXID + 32].copy_from_slice(&spent_by.txid.to_byte_array());
        buf[R_SPENT_HEIGHT..R_SPENT_HEIGHT + 4]
            .copy_from_slice(&spent_by.block_height.to_le_bytes());
    }

    Ok(buf)
}

fn decode_record(buf: &[u8; RECORD_LEN]) -> Result<(OutPoint, Coin), WalletPersistenceError> {
    let spent_flag = buf[R_SPENT_FLAG];
    if spent_flag != SPENT_FLAG_UNSPENT && spent_flag != SPENT_FLAG_SPENT {
        return Err(WalletPersistenceError::UnknownSpentFlag(spent_flag));
    }

    let mut txid_bytes = [0u8; 32];
    txid_bytes.copy_from_slice(&buf[R_TXID..R_TXID + 32]);
    let txid = Txid::from_byte_array(txid_bytes);
    let vout = u32::from_le_bytes(buf[R_VOUT..R_VOUT + 4].try_into().unwrap());
    let outpoint = OutPoint { txid, vout };

    let value = Amount::from_sat(u64::from_le_bytes(
        buf[R_VALUE..R_VALUE + 8].try_into().unwrap(),
    ));
    let script_pubkey = ScriptBuf::from(buf[R_SCRIPT..R_SCRIPT + SCRIPT_LEN].to_vec());

    let tweak_bytes: [u8; 32] = buf[R_TWEAK..R_TWEAK + 32].try_into().unwrap();
    let tweak =
        Scalar::from_be_bytes(tweak_bytes).map_err(|_| WalletPersistenceError::InvalidScalar)?;

    let label = match buf[R_LABEL_TAG] {
        LABEL_TAG_NONE => None,
        LABEL_TAG_SOME => {
            let label_bytes: [u8; 32] =
                buf[R_LABEL_SCALAR..R_LABEL_SCALAR + 32].try_into().unwrap();
            Some(
                Scalar::from_be_bytes(label_bytes)
                    .map_err(|_| WalletPersistenceError::InvalidScalar)?,
            )
        }
        other => return Err(WalletPersistenceError::UnknownLabelTag(other)),
    };

    let block_height =
        u32::from_le_bytes(buf[R_BLOCK_HEIGHT..R_BLOCK_HEIGHT + 4].try_into().unwrap());

    let spent_by = if spent_flag == SPENT_FLAG_SPENT {
        let mut spent_txid_bytes = [0u8; 32];
        spent_txid_bytes.copy_from_slice(&buf[R_SPENT_TXID..R_SPENT_TXID + 32]);
        let spent_height =
            u32::from_le_bytes(buf[R_SPENT_HEIGHT..R_SPENT_HEIGHT + 4].try_into().unwrap());
        Some(SpentBy {
            txid: Txid::from_byte_array(spent_txid_bytes),
            block_height: spent_height,
        })
    } else {
        None
    };

    Ok((
        outpoint,
        Coin {
            value,
            script_pubkey,
            tweak,
            label,
            block_height,
            spent_by,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p2tr_script(seed: u8) -> ScriptBuf {
        let mut bytes = vec![0x51u8, 0x20];
        bytes.extend_from_slice(&[seed; 32]);
        ScriptBuf::from(bytes)
    }

    fn outpoint_n(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout: n as u32,
        }
    }

    fn coin(value_sats: u64, n: u8, block_height: u32) -> Coin {
        Coin {
            value: Amount::from_sat(value_sats),
            script_pubkey: p2tr_script(n),
            tweak: Scalar::from_be_bytes([n; 32]).unwrap(),
            label: None,
            block_height,
            spent_by: None,
        }
    }

    fn spent_coin(value_sats: u64, n: u8, block_height: u32, spent_at: u32, spender: u8) -> Coin {
        Coin {
            value: Amount::from_sat(value_sats),
            script_pubkey: p2tr_script(n),
            tweak: Scalar::from_be_bytes([n; 32]).unwrap(),
            label: None,
            block_height,
            spent_by: Some(SpentBy {
                txid: Txid::from_byte_array([spender; 32]),
                block_height: spent_at,
            }),
        }
    }

    fn populated_wallet() -> Wallet {
        let mut w = Wallet::new(Network::Regtest);
        w.scan_height = 200;
        w.utxos.insert(outpoint_n(1), coin(50_000, 1, 100));
        w.utxos
            .insert(outpoint_n(2), spent_coin(30_000, 2, 101, 150, 9));
        w
    }

    fn assert_wallets_match(a: &Wallet, b: &Wallet) {
        assert_eq!(a.scan_height, b.scan_height);
        assert_eq!(a.network, b.network);
        assert_eq!(a.utxos.len(), b.utxos.len());
        for (op, coin_a) in &a.utxos {
            let coin_b = b.utxos.get(op).expect("outpoint present in both");
            assert_eq!(coin_a, coin_b);
        }
    }

    #[test]
    fn to_bytes_then_from_bytes_round_trips() {
        let original = populated_wallet();
        let bytes = wallet_to_bytes(&original).unwrap();
        let decoded = wallet_from_bytes(&bytes).unwrap();
        assert_wallets_match(&original, &decoded);
    }

    #[test]
    fn empty_wallet_round_trips() {
        let original = Wallet::new(Network::Mainnet);
        let bytes = wallet_to_bytes(&original).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + COUNT_LEN);
        let decoded = wallet_from_bytes(&bytes).unwrap();
        assert_wallets_match(&original, &decoded);
    }

    #[test]
    fn save_load_through_disk_round_trips() {
        let original = populated_wallet();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        original.save(&path).unwrap();
        let decoded = Wallet::load(&path).unwrap();
        assert_wallets_match(&original, &decoded);
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let mut first = Wallet::new(Network::Regtest);
        first.utxos.insert(outpoint_n(1), coin(50_000, 1, 100));
        first.save(&path).unwrap();

        let mut second = Wallet::new(Network::Regtest);
        second.scan_height = 500;
        second.utxos.insert(outpoint_n(2), coin(70_000, 2, 200));
        second.save(&path).unwrap();

        let decoded = Wallet::load(&path).unwrap();
        assert_wallets_match(&second, &decoded);
    }

    #[test]
    fn save_does_not_leave_tmp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        populated_wallet().save(&path).unwrap();

        let mut tmp_os = path.as_os_str().to_owned();
        tmp_os.push(".tmp");
        let tmp: PathBuf = tmp_os.into();
        assert!(!tmp.exists(), "tmp file should have been renamed");
    }

    #[test]
    fn from_bytes_rejects_bad_magic() {
        let mut bytes = wallet_to_bytes(&populated_wallet()).unwrap();
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(matches!(
            wallet_from_bytes(&bytes),
            Err(WalletPersistenceError::BadMagic)
        ));
    }

    #[test]
    fn from_bytes_rejects_wrong_version() {
        let mut bytes = wallet_to_bytes(&populated_wallet()).unwrap();
        bytes[4] = 99;
        assert!(matches!(
            wallet_from_bytes(&bytes),
            Err(WalletPersistenceError::WrongVersion(99))
        ));
    }

    #[test]
    fn from_bytes_rejects_unknown_network() {
        let mut bytes = wallet_to_bytes(&populated_wallet()).unwrap();
        bytes[5] = 99;
        assert!(matches!(
            wallet_from_bytes(&bytes),
            Err(WalletPersistenceError::UnknownNetwork(99))
        ));
    }

    #[test]
    fn from_bytes_rejects_truncated_header() {
        assert!(matches!(
            wallet_from_bytes(b"SPWF\x01\x02"),
            Err(WalletPersistenceError::TruncatedFile)
        ));
    }

    #[test]
    fn from_bytes_rejects_truncated_records() {
        let mut bytes = wallet_to_bytes(&populated_wallet()).unwrap();
        bytes.truncate(bytes.len() - 10);
        assert!(matches!(
            wallet_from_bytes(&bytes),
            Err(WalletPersistenceError::TruncatedFile)
        ));
    }

    #[test]
    fn from_bytes_rejects_unknown_spent_flag() {
        let mut bytes = wallet_to_bytes(&populated_wallet()).unwrap();
        let first_record_off = HEADER_LEN + COUNT_LEN;
        bytes[first_record_off + R_SPENT_FLAG] = 99;
        assert!(matches!(
            wallet_from_bytes(&bytes),
            Err(WalletPersistenceError::UnknownSpentFlag(99))
        ));
    }

    #[test]
    fn loading_yields_no_keys() {
        let original = populated_wallet();
        let bytes = wallet_to_bytes(&original).unwrap();
        let decoded = wallet_from_bytes(&bytes).unwrap();
        assert!(decoded.keys.is_none());
        assert!(decoded.spend_key.is_none());
    }

    #[test]
    fn network_round_trips() {
        for net in [Network::Mainnet, Network::Testnet, Network::Regtest] {
            let w = Wallet::new(net);
            let bytes = wallet_to_bytes(&w).unwrap();
            let decoded = wallet_from_bytes(&bytes).unwrap();
            assert_eq!(decoded.network, net);
        }
    }

    fn tmp_store(network: Network) -> (tempfile::TempDir, WalletStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        let store = WalletStore::new(path, network);
        (dir, store)
    }

    #[test]
    fn store_exists_is_false_before_save() {
        let (_dir, store) = tmp_store(Network::Regtest);
        assert!(!store.exists());
    }

    #[test]
    fn store_save_then_exists_is_true() {
        let (_dir, store) = tmp_store(Network::Regtest);
        let wallet = Wallet::new(Network::Regtest);
        store.save(&wallet).unwrap();
        assert!(store.exists());
    }

    #[test]
    fn store_save_then_load_round_trips() {
        let (_dir, store) = tmp_store(Network::Regtest);
        let wallet = Wallet::new(Network::Regtest);
        store.save(&wallet).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.network, Network::Regtest);
        assert_eq!(loaded.scan_height, 0);
    }

    #[test]
    fn store_load_returns_network_mismatch_when_file_for_other_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let regtest_store = WalletStore::new(path.clone(), Network::Regtest);
        let regtest_wallet = Wallet::new(Network::Regtest);
        regtest_store.save(&regtest_wallet).unwrap();

        let mainnet_store = WalletStore::new(path, Network::Mainnet);
        let err = mainnet_store.load().expect_err("network mismatch");
        assert!(matches!(
            err,
            WalletStoreError::NetworkMismatch {
                file: Network::Regtest,
                expected: Network::Mainnet
            }
        ));
    }

    #[test]
    fn store_load_propagates_persistence_error_for_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        let mut bytes = b"XXXX".to_vec();
        bytes.extend_from_slice(&[0u8; 10]);
        std::fs::write(&path, bytes).unwrap();

        let store = WalletStore::new(path, Network::Regtest);
        let err = store.load().expect_err("bad magic");
        assert!(matches!(
            err,
            WalletStoreError::Persistence(WalletPersistenceError::BadMagic)
        ));
    }
}
