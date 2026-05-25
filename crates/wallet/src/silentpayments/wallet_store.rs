use std::{
    collections::HashMap,
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use bitcoin::{hashes::Hash, secp256k1::Scalar, Amount, OutPoint, ScriptBuf, Txid};

use crate::silentpayments::{
    wallet::{Coin, SpentBy},
    Network,
};

// ─── header layout (14 bytes) ─────────────────────────────────────────────────
//   [0..4]  magic       b"SPWF"
//   [4]     version     u8
//   [5]     network     u8  0=Mainnet 1=Testnet 2=Regtest
//   [6..10] scan_height u32 le
//   [10..14] total_records u32 le   (includes tombstones)

const MAGIC: &[u8; 4] = b"SPWF";
const VERSION: u8 = 1;
const HEADER_LEN: u64 = 14;

const H_SCAN_HEIGHT: u64 = 6;
const H_TOTAL_RECORDS: u64 = 10;

// ─── record layout (184 bytes, fixed) ─────────────────────────────────────────
//
//   Silent payments always produce P2TR outputs (34-byte scripts), so every
//   field is fixed-width and records can be addressed by index without a scan.
//
//   [0]        status       u8   0=active 1=spent 2=tombstone
//   [1..33]    txid         [u8; 32]
//   [33..37]   vout         u32 le
//   [37..45]   value        u64 le   (satoshis)
//   [45..79]   script       [u8; 34]
//   [79..111]  tweak        [u8; 32]  (Scalar, big-endian)
//   [111]      label_tag    u8   0=None 1=Some
//   [112..144] label_scalar [u8; 32]  (zeroed when label_tag=0)
//   [144..148] block_height u32 le
//   [148..180] spent_txid   [u8; 32]  (zeroed when status != 1)
//   [180..184] spent_height u32 le    (0 when status != 1)

pub const RECORD_LEN: u64 = 184;
const RECORD_SIZE: usize = 184;
const SCRIPT_LEN: usize = 34;

const R_STATUS: u64 = 0;
const R_TXID: u64 = 1;
const R_VOUT: u64 = 33;
const R_VALUE: u64 = 37;
const R_SCRIPT: u64 = 45;
const R_TWEAK: u64 = 79;
const R_LABEL_TAG: u64 = 111;
const R_LABEL_SCALAR: u64 = 112;
const R_BLOCK_HEIGHT: u64 = 144;
const R_SPENT_TXID: u64 = 148;
const R_SPENT_HEIGHT: u64 = 180;

const STATUS_ACTIVE: u8 = 0;
const STATUS_SPENT: u8 = 1;
const STATUS_TOMBSTONE: u8 = 2;

// ─── error ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum WalletStoreError {
    Io(io::Error),
    BadMagic,
    WrongVersion(u8),
    UnknownNetwork(u8),
    InvalidScalar,
    NonP2trScript { got: usize },
}

impl fmt::Display for WalletStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o: {e}"),
            Self::BadMagic => write!(f, "bad magic bytes"),
            Self::WrongVersion(v) => write!(f, "unsupported version: {v}"),
            Self::UnknownNetwork(n) => write!(f, "unknown network byte: {n}"),
            Self::InvalidScalar => write!(f, "invalid scalar in record"),
            Self::NonP2trScript { got } => {
                write!(f, "script_pubkey is {got} bytes, expected {SCRIPT_LEN}")
            }
        }
    }
}

impl std::error::Error for WalletStoreError {}

impl From<io::Error> for WalletStoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ─── store ────────────────────────────────────────────────────────────────────

/// Durable backing store for wallet state using fixed-size coin records.
///
/// The store keeps the file open and maintains an in-memory index mapping each
/// outpoint to its record number. Writes are surgical:
/// - new coins: appended at the end (O(1) per coin)
/// - spent updates: in-place at the record's fixed offset (O(1))
/// - tombstones (reorg removes): single-byte status update (O(1))
/// - scan_height: 4-byte header update (O(1))
pub struct WalletStore {
    file: File,
    /// outpoint → record index (0-based); rebuilt from file on `open`
    index: HashMap<OutPoint, u64>,
    pub network: Network,
    pub scan_height: u32,
    total_records: u64,
}

impl WalletStore {
    /// Creates a new store file. Fails if the file already exists.
    pub fn create(path: &Path, network: Network) -> Result<Self, WalletStoreError> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        let mut header = [0u8; HEADER_LEN as usize];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = VERSION;
        header[5] = encode_network(network);
        // bytes 6-13: scan_height and total_records stay zero
        file.write_all(&header)?;

        Ok(Self {
            file,
            index: HashMap::new(),
            network,
            scan_height: 0,
            total_records: 0,
        })
    }

    /// Opens an existing store and rebuilds the in-memory index.
    pub fn open(path: &Path) -> Result<Self, WalletStoreError> {
        let mut file = File::options().read(true).write(true).open(path)?;

        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)?;

        if &header[0..4] != MAGIC.as_slice() {
            return Err(WalletStoreError::BadMagic);
        }
        if header[4] != VERSION {
            return Err(WalletStoreError::WrongVersion(header[4]));
        }
        let network = decode_network(header[5])?;
        let scan_height = u32::from_le_bytes(header[6..10].try_into().unwrap());
        let total_records = u32::from_le_bytes(header[10..14].try_into().unwrap()) as u64;

        let mut index = HashMap::with_capacity(total_records as usize);
        let mut status_buf = [0u8; 1];
        let mut op_buf = [0u8; 36]; // txid (32) + vout (4)

        for i in 0..total_records {
            let base = HEADER_LEN + i * RECORD_LEN;

            file.seek(SeekFrom::Start(base + R_STATUS))?;
            file.read_exact(&mut status_buf)?;
            if status_buf[0] == STATUS_TOMBSTONE {
                continue;
            }

            file.seek(SeekFrom::Start(base + R_TXID))?;
            file.read_exact(&mut op_buf)?;
            let txid = Txid::from_byte_array(op_buf[0..32].try_into().unwrap());
            let vout = u32::from_le_bytes(op_buf[32..36].try_into().unwrap());
            index.insert(OutPoint { txid, vout }, i);
        }

        Ok(Self { file, index, network, scan_height, total_records })
    }

    /// Returns all non-tombstone records (both active and spent), in file order.
    /// Used to restore the wallet's in-memory UTXO map on startup.
    pub fn coins(&mut self) -> Result<Vec<(OutPoint, Coin)>, WalletStoreError> {
        let mut result = Vec::with_capacity(self.index.len());
        let mut buf = [0u8; RECORD_SIZE];

        for i in 0..self.total_records {
            self.file.seek(SeekFrom::Start(HEADER_LEN + i * RECORD_LEN))?;
            self.file.read_exact(&mut buf)?;
            if buf[R_STATUS as usize] == STATUS_TOMBSTONE {
                continue;
            }
            result.push(decode_record(&buf)?);
        }

        Ok(result)
    }

    /// Persists a completed block scan: appends new coins and marks spent outputs.
    ///
    /// Write ordering for crash safety:
    /// - New records are written before `total_records` header update.
    /// - Spent data fields are written before the status byte flip.
    /// - `scan_height` is updated last.
    ///
    /// On a crash mid-write the worst case is re-processing the last block,
    /// which is idempotent: duplicate inserts and re-marks are no-ops.
    pub fn apply_scan(
        &mut self,
        height: u32,
        new_coins: &[(OutPoint, Coin)],
        newly_spent: &[(OutPoint, SpentBy)],
    ) -> Result<(), WalletStoreError> {
        // Append new coins first so that same-block spends can find them below.
        if !new_coins.is_empty() {
            self.file
                .seek(SeekFrom::Start(HEADER_LEN + self.total_records * RECORD_LEN))?;
            let count_before = self.total_records;

            for (outpoint, coin) in new_coins {
                if self.index.contains_key(outpoint) {
                    continue; // idempotent: skip already-persisted coins
                }
                self.file.write_all(&encode_record(outpoint, coin)?)?;
                self.index.insert(*outpoint, self.total_records);
                self.total_records += 1;
            }

            if self.total_records > count_before {
                self.file.seek(SeekFrom::Start(H_TOTAL_RECORDS))?;
                self.file.write_all(&(self.total_records as u32).to_le_bytes())?;
            }
        }

        // Mark spent: write data fields first, flip status byte last.
        for (outpoint, spent_by) in newly_spent {
            let Some(&record_idx) = self.index.get(outpoint) else {
                continue;
            };
            let base = HEADER_LEN + record_idx * RECORD_LEN;

            self.file.seek(SeekFrom::Start(base + R_SPENT_TXID))?;
            self.file.write_all(&spent_by.txid.to_byte_array())?;
            self.file.write_all(&spent_by.block_height.to_le_bytes())?;

            self.file.seek(SeekFrom::Start(base + R_STATUS))?;
            self.file.write_all(&[STATUS_SPENT])?;
        }

        self.scan_height = height;
        self.file.seek(SeekFrom::Start(H_SCAN_HEIGHT))?;
        self.file.write_all(&height.to_le_bytes())?;

        Ok(())
    }

    /// Persists a chain disconnect: tombstones removed coins and restores
    /// spent-by state for coins un-spent by the disconnected block.
    ///
    /// Write ordering: zero spent fields before clearing the status byte,
    /// so a crash leaves at most a coin with garbage-but-ignored spent fields.
    pub fn apply_disconnect(
        &mut self,
        new_height: u32,
        removed: &[OutPoint],
        unspent: &[OutPoint],
    ) -> Result<(), WalletStoreError> {
        for outpoint in removed {
            let Some(record_idx) = self.index.remove(outpoint) else {
                continue;
            };
            let base = HEADER_LEN + record_idx * RECORD_LEN;
            self.file.seek(SeekFrom::Start(base + R_STATUS))?;
            self.file.write_all(&[STATUS_TOMBSTONE])?;
        }

        for outpoint in unspent {
            let Some(&record_idx) = self.index.get(outpoint) else {
                continue;
            };
            let base = HEADER_LEN + record_idx * RECORD_LEN;
            // Zero the 36 spent bytes (txid + height), then clear the status.
            self.file.seek(SeekFrom::Start(base + R_SPENT_TXID))?;
            self.file.write_all(&[0u8; 36])?;
            self.file.seek(SeekFrom::Start(base + R_STATUS))?;
            self.file.write_all(&[STATUS_ACTIVE])?;
        }

        self.scan_height = new_height;
        self.file.seek(SeekFrom::Start(H_SCAN_HEIGHT))?;
        self.file.write_all(&new_height.to_le_bytes())?;

        Ok(())
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn encode_network(n: Network) -> u8 {
    match n {
        Network::Mainnet => 0,
        Network::Testnet => 1,
        Network::Regtest => 2,
    }
}

fn decode_network(b: u8) -> Result<Network, WalletStoreError> {
    match b {
        0 => Ok(Network::Mainnet),
        1 => Ok(Network::Testnet),
        2 => Ok(Network::Regtest),
        n => Err(WalletStoreError::UnknownNetwork(n)),
    }
}

/// Encodes a new (unspent) coin as a 184-byte record with `STATUS_ACTIVE`.
/// Spent fields are zeroed; they are updated in-place by `apply_scan`.
fn encode_record(
    outpoint: &OutPoint,
    coin: &Coin,
) -> Result<[u8; RECORD_SIZE], WalletStoreError> {
    let script = coin.script_pubkey.as_bytes();
    if script.len() != SCRIPT_LEN {
        return Err(WalletStoreError::NonP2trScript { got: script.len() });
    }

    let mut buf = [0u8; RECORD_SIZE];

    buf[R_STATUS as usize] = STATUS_ACTIVE;
    buf[R_TXID as usize..R_TXID as usize + 32]
        .copy_from_slice(&outpoint.txid.to_byte_array());
    buf[R_VOUT as usize..R_VOUT as usize + 4]
        .copy_from_slice(&outpoint.vout.to_le_bytes());
    buf[R_VALUE as usize..R_VALUE as usize + 8]
        .copy_from_slice(&coin.value.to_sat().to_le_bytes());
    buf[R_SCRIPT as usize..R_SCRIPT as usize + SCRIPT_LEN].copy_from_slice(script);
    buf[R_TWEAK as usize..R_TWEAK as usize + 32].copy_from_slice(&coin.tweak[..]);

    if let Some(ref label_scalar) = coin.label {
        buf[R_LABEL_TAG as usize] = 1;
        buf[R_LABEL_SCALAR as usize..R_LABEL_SCALAR as usize + 32]
            .copy_from_slice(&label_scalar[..]);
    }

    buf[R_BLOCK_HEIGHT as usize..R_BLOCK_HEIGHT as usize + 4]
        .copy_from_slice(&coin.block_height.to_le_bytes());

    Ok(buf)
}

fn decode_record(buf: &[u8; RECORD_SIZE]) -> Result<(OutPoint, Coin), WalletStoreError> {
    let status = buf[R_STATUS as usize];

    let txid =
        Txid::from_byte_array(buf[R_TXID as usize..R_TXID as usize + 32].try_into().unwrap());
    let vout = u32::from_le_bytes(buf[R_VOUT as usize..R_VOUT as usize + 4].try_into().unwrap());
    let outpoint = OutPoint { txid, vout };

    let value = Amount::from_sat(u64::from_le_bytes(
        buf[R_VALUE as usize..R_VALUE as usize + 8].try_into().unwrap(),
    ));

    let script_pubkey =
        ScriptBuf::from(buf[R_SCRIPT as usize..R_SCRIPT as usize + SCRIPT_LEN].to_vec());

    let tweak_bytes: [u8; 32] = buf[R_TWEAK as usize..R_TWEAK as usize + 32].try_into().unwrap();
    let tweak =
        Scalar::from_be_bytes(tweak_bytes).map_err(|_| WalletStoreError::InvalidScalar)?;

    let label = if buf[R_LABEL_TAG as usize] == 1 {
        let label_bytes: [u8; 32] = buf[R_LABEL_SCALAR as usize..R_LABEL_SCALAR as usize + 32]
            .try_into()
            .unwrap();
        Some(Scalar::from_be_bytes(label_bytes).map_err(|_| WalletStoreError::InvalidScalar)?)
    } else {
        None
    };

    let block_height = u32::from_le_bytes(
        buf[R_BLOCK_HEIGHT as usize..R_BLOCK_HEIGHT as usize + 4]
            .try_into()
            .unwrap(),
    );

    let spent_by = if status == STATUS_SPENT {
        let spent_txid = Txid::from_byte_array(
            buf[R_SPENT_TXID as usize..R_SPENT_TXID as usize + 32]
                .try_into()
                .unwrap(),
        );
        let spent_height = u32::from_le_bytes(
            buf[R_SPENT_HEIGHT as usize..R_SPENT_HEIGHT as usize + 4]
                .try_into()
                .unwrap(),
        );
        Some(SpentBy { txid: spent_txid, block_height: spent_height })
    } else {
        None
    };

    Ok((outpoint, Coin { value, script_pubkey, tweak, label, block_height, spent_by }))
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_outpoint(n: u8) -> OutPoint {
        OutPoint { txid: Txid::from_byte_array([n; 32]), vout: n as u32 }
    }

    fn p2tr_script() -> ScriptBuf {
        // OP_1 OP_PUSHBYTES_32 <32 zero bytes> — valid 34-byte P2TR scriptpubkey shape
        let mut bytes = vec![0x51u8, 0x20];
        bytes.extend_from_slice(&[0u8; 32]);
        ScriptBuf::from(bytes)
    }

    fn dummy_coin() -> Coin {
        Coin {
            value: Amount::from_sat(100_000),
            script_pubkey: p2tr_script(),
            tweak: Scalar::from_be_bytes([1u8; 32]).unwrap(),
            label: None,
            block_height: 800_000,
            spent_by: None,
        }
    }

    #[test]
    fn create_then_open_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let store = WalletStore::create(&path, Network::Regtest).unwrap();
        assert_eq!(store.scan_height, 0);
        assert_eq!(store.total_records, 0);
        drop(store);

        let store = WalletStore::open(&path).unwrap();
        assert_eq!(store.scan_height, 0);
        assert!(matches!(store.network, Network::Regtest));
    }

    #[test]
    fn create_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        WalletStore::create(&path, Network::Regtest).unwrap();
        let result = WalletStore::create(&path, Network::Regtest);
        assert!(matches!(
            result,
            Err(WalletStoreError::Io(ref e)) if e.kind() == io::ErrorKind::AlreadyExists
        ));
    }

    #[test]
    fn new_coins_persisted_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let coin = dummy_coin();

        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, coin.clone())], &[]).unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        assert_eq!(store.scan_height, 100);
        let coins = store.coins().unwrap();
        assert_eq!(coins.len(), 1);
        assert_eq!(coins[0].0, op);
        assert_eq!(coins[0].1.value, coin.value);
        assert_eq!(coins[0].1.block_height, coin.block_height);
        assert!(coins[0].1.spent_by.is_none());
    }

    #[test]
    fn mark_spent_persisted_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let spending_txid = Txid::from_byte_array([9u8; 32]);

        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, dummy_coin())], &[]).unwrap();
        store
            .apply_scan(101, &[], &[(op, SpentBy { txid: spending_txid, block_height: 101 })])
            .unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        let coins = store.coins().unwrap();
        assert_eq!(coins.len(), 1);
        let s = coins[0].1.spent_by.as_ref().unwrap();
        assert_eq!(s.txid, spending_txid);
        assert_eq!(s.block_height, 101);
    }

    #[test]
    fn same_block_receive_and_spend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let spending_txid = Txid::from_byte_array([9u8; 32]);

        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store
            .apply_scan(
                100,
                &[(op, dummy_coin())],
                &[(op, SpentBy { txid: spending_txid, block_height: 100 })],
            )
            .unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        let coins = store.coins().unwrap();
        assert_eq!(coins.len(), 1);
        assert!(coins[0].1.spent_by.is_some());
    }

    #[test]
    fn disconnect_tombstones_coin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, dummy_coin())], &[]).unwrap();
        store.apply_disconnect(99, &[op], &[]).unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        assert_eq!(store.scan_height, 99);
        assert!(store.coins().unwrap().is_empty());
    }

    #[test]
    fn disconnect_clears_spent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, dummy_coin())], &[]).unwrap();
        store
            .apply_scan(101, &[], &[(op, SpentBy { txid: Txid::from_byte_array([9u8; 32]), block_height: 101 })])
            .unwrap();
        store.apply_disconnect(100, &[], &[op]).unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        let coins = store.coins().unwrap();
        assert_eq!(coins.len(), 1);
        assert!(coins[0].1.spent_by.is_none());
    }

    #[test]
    fn label_scalar_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let label_scalar = Scalar::from_be_bytes([3u8; 32]).unwrap();
        let coin = Coin { label: Some(label_scalar), ..dummy_coin() };

        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, coin)], &[]).unwrap();
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        let coins = store.coins().unwrap();
        let loaded_label = coins[0].1.label.as_ref().unwrap();
        assert_eq!(&loaded_label[..], &label_scalar[..]);
    }

    #[test]
    fn open_rejects_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");
        std::fs::write(&path, b"XXXX\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        assert!(matches!(WalletStore::open(&path), Err(WalletStoreError::BadMagic)));
    }

    #[test]
    fn apply_scan_is_idempotent_for_duplicate_coins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.bin");

        let op = dummy_outpoint(1);
        let mut store = WalletStore::create(&path, Network::Regtest).unwrap();
        store.apply_scan(100, &[(op, dummy_coin())], &[]).unwrap();
        store.apply_scan(100, &[(op, dummy_coin())], &[]).unwrap(); // re-applied
        drop(store);

        let mut store = WalletStore::open(&path).unwrap();
        assert_eq!(store.coins().unwrap().len(), 1);
    }
}
