use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use bitcoin::secp256k1::{self, Secp256k1, SecretKey, XOnlyPublicKey};

use crate::io::FileExt;

const MAGIC: &[u8; 4] = b"SPKF";
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 1;
const FILE_LEN: usize = MAGIC.len() + KEY_LEN + TAG_LEN + KEY_LEN;

const SPEND_TAG_SECRET: u8 = 0;
const SPEND_TAG_XONLY_PUB: u8 = 1;

/// `Secret` for software wallets. `XOnlyPublic` for hardware-signer / watch-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendKey {
    Secret(SecretKey),
    XOnlyPublic(XOnlyPublicKey),
}

impl SpendKey {
    pub fn xonly(&self) -> XOnlyPublicKey {
        match self {
            SpendKey::Secret(sk) => {
                let secp = Secp256k1::signing_only();
                sk.public_key(&secp).x_only_public_key().0
            }
            SpendKey::XOnlyPublic(pk) => *pk,
        }
    }
}

/// On-disk format: 4-byte magic `SPKF` (Silent Payment Key File), 32-byte scan
/// secret, 1-byte spend tag, 32-byte spend material. 69 bytes total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilentPaymentKeysFile {
    pub scan_key: SecretKey,
    pub spend: SpendKey,
}

#[derive(Debug)]
pub enum KeysFileError {
    Io(io::Error),
    BadMagic,
    WrongLength { expected: usize, actual: usize },
    UnknownSpendTag(u8),
    InvalidKey(secp256k1::Error),
}

impl std::fmt::Display for KeysFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeysFileError::Io(e) => write!(f, "i/o error: {e}"),
            KeysFileError::BadMagic => write!(f, "bad magic: not a silent payments keys file"),
            KeysFileError::WrongLength { expected, actual } => write!(
                f,
                "wrong file length: expected {expected} bytes, got {actual}"
            ),
            KeysFileError::UnknownSpendTag(t) => write!(f, "unknown spend tag: {t}"),
            KeysFileError::InvalidKey(e) => write!(f, "invalid key: {e}"),
        }
    }
}

impl std::error::Error for KeysFileError {}

impl From<io::Error> for KeysFileError {
    fn from(e: io::Error) -> Self {
        KeysFileError::Io(e)
    }
}

impl From<secp256k1::Error> for KeysFileError {
    fn from(e: secp256k1::Error) -> Self {
        KeysFileError::InvalidKey(e)
    }
}

impl SilentPaymentKeysFile {
    pub fn new(scan_key: SecretKey, spend: SpendKey) -> Self {
        Self { scan_key, spend }
    }

    pub fn spend_xonly(&self) -> XOnlyPublicKey {
        self.spend.xonly()
    }

    pub fn to_bytes(&self) -> [u8; FILE_LEN] {
        let mut buf = [0u8; FILE_LEN];
        let mut off = 0;
        buf[off..off + MAGIC.len()].copy_from_slice(MAGIC);
        off += MAGIC.len();
        buf[off..off + KEY_LEN].copy_from_slice(&self.scan_key.secret_bytes());
        off += KEY_LEN;
        match &self.spend {
            SpendKey::Secret(sk) => {
                buf[off] = SPEND_TAG_SECRET;
                buf[off + TAG_LEN..].copy_from_slice(&sk.secret_bytes());
            }
            SpendKey::XOnlyPublic(pk) => {
                buf[off] = SPEND_TAG_XONLY_PUB;
                buf[off + TAG_LEN..].copy_from_slice(&pk.serialize());
            }
        }
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeysFileError> {
        if bytes.len() != FILE_LEN {
            return Err(KeysFileError::WrongLength {
                expected: FILE_LEN,
                actual: bytes.len(),
            });
        }
        let body = bytes
            .strip_prefix(MAGIC.as_slice())
            .ok_or(KeysFileError::BadMagic)?;
        let scan_key = SecretKey::from_slice(&body[..KEY_LEN])?;
        let tag = body[KEY_LEN];
        let spend_bytes = &body[KEY_LEN + TAG_LEN..];
        let spend = match tag {
            SPEND_TAG_SECRET => SpendKey::Secret(SecretKey::from_slice(spend_bytes)?),
            SPEND_TAG_XONLY_PUB => SpendKey::XOnlyPublic(XOnlyPublicKey::from_slice(spend_bytes)?),
            other => return Err(KeysFileError::UnknownSpendTag(other)),
        };
        Ok(Self { scan_key, spend })
    }
}

impl FileExt for SilentPaymentKeysFile {
    type Error = KeysFileError;

    /// Refuses to overwrite (`AlreadyExists`).
    fn save(&self, path: &Path) -> Result<(), Self::Error> {
        let mut file = fs::File::create_new(path)?;
        file.write_all(&self.to_bytes())?;
        Ok(())
    }

    fn load(path: &Path) -> Result<Self, Self::Error> {
        let bytes = fs::read(path)?;
        Self::from_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keys() -> (SecretKey, SecretKey) {
        let scan = SecretKey::from_slice(&[1u8; 32]).expect("valid scan key");
        let spend = SecretKey::from_slice(&[2u8; 32]).expect("valid spend key");
        (scan, spend)
    }

    fn xonly_from(sk: SecretKey) -> XOnlyPublicKey {
        sk.public_key(&Secp256k1::signing_only())
            .x_only_public_key()
            .0
    }

    #[test]
    fn roundtrip_preserves_secret_variant() {
        let (scan, spend) = test_keys();
        let original = SilentPaymentKeysFile::new(scan, SpendKey::Secret(spend));
        let bytes = original.to_bytes();
        let decoded = SilentPaymentKeysFile::from_bytes(&bytes).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn roundtrip_preserves_xonly_public_variant() {
        let (scan, spend) = test_keys();
        let xonly = xonly_from(spend);
        let original = SilentPaymentKeysFile::new(scan, SpendKey::XOnlyPublic(xonly));
        let bytes = original.to_bytes();
        let decoded = SilentPaymentKeysFile::from_bytes(&bytes).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn from_bytes_rejects_bad_magic() {
        let (scan, spend) = test_keys();
        let mut bytes = b"XXXX".to_vec();
        bytes.extend_from_slice(&scan.secret_bytes());
        bytes.push(SPEND_TAG_SECRET);
        bytes.extend_from_slice(&spend.secret_bytes());
        assert!(matches!(
            SilentPaymentKeysFile::from_bytes(&bytes),
            Err(KeysFileError::BadMagic)
        ));
    }

    #[test]
    fn from_bytes_rejects_unknown_spend_tag() {
        let (scan, spend) = test_keys();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&scan.secret_bytes());
        bytes.push(99);
        bytes.extend_from_slice(&spend.secret_bytes());
        assert!(matches!(
            SilentPaymentKeysFile::from_bytes(&bytes),
            Err(KeysFileError::UnknownSpendTag(99))
        ));
    }

    #[test]
    fn save_load_roundtrips_through_disk() {
        let (scan, spend) = test_keys();
        let original = SilentPaymentKeysFile::new(scan, SpendKey::Secret(spend));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.bin");
        original.save(&path).expect("save");
        let decoded = SilentPaymentKeysFile::load(&path).expect("load");
        assert_eq!(original, decoded);
    }

    #[test]
    fn save_refuses_to_overwrite_existing_file() {
        let (scan, spend) = test_keys();
        let file = SilentPaymentKeysFile::new(scan, SpendKey::Secret(spend));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.bin");
        file.save(&path).expect("first save");
        let err = file.save(&path).expect_err("second save must fail");
        assert!(
            matches!(&err, KeysFileError::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
            "expected Io(AlreadyExists), got {err:?}"
        );
    }
}
