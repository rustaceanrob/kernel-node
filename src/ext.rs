use bitcoin::{consensus::encode, hashes::Hash, Network};
use bitcoinkernel::{core::BlockHashExt, Block as KernelBlock, BlockTreeEntry, ChainType};
use std::{fs, path::PathBuf};
use wallet::silentpayments::Network as WalletNetwork;

pub trait ChainExt {
    fn chain_type(&self) -> ChainType;
}

impl ChainExt for Network {
    fn chain_type(&self) -> ChainType {
        match self {
            Network::Bitcoin => ChainType::Mainnet,
            Network::Signet => ChainType::Signet,
            Network::Testnet => ChainType::Testnet,
            Network::Testnet4 => ChainType::Testnet4,
            Network::Regtest => ChainType::Regtest,
        }
    }
}

fn unix_home_dir() -> PathBuf {
    let home_dir_string = std::env::var("HOME").unwrap();
    home_dir_string.parse::<PathBuf>().unwrap()
}

pub trait DirnameExt {
    fn data_dir(&self) -> String;
}

impl<S: AsRef<str>> DirnameExt for S {
    fn data_dir(&self) -> String {
        let string = self.as_ref();
        let path = match string.strip_prefix("~/") {
            Some(rest) => {
                let home_path = unix_home_dir();
                home_path.join(rest)
            }
            None => PathBuf::from(string),
        };
        // Create directories if they don't exist
        fs::create_dir_all(&path).unwrap();

        // Get canonical (full) path
        path.canonicalize().unwrap().to_str().unwrap().to_string()
    }
}

pub trait NetworkExt {
    // The P2P port for a given [`Network`].
    fn default_p2p_port(self) -> u16;
    fn wallet_network(self) -> WalletNetwork;
}

impl NetworkExt for Network {
    // The P2P port for a given [`Network`].
    fn default_p2p_port(self) -> u16 {
        match &self {
            Self::Bitcoin => 8333,
            Self::Signet => 38333,
            Self::Testnet => 18333,
            Self::Testnet4 => 48333,
            Self::Regtest => 18444,
        }
    }

    fn wallet_network(self) -> WalletNetwork {
        match self {
            Self::Bitcoin => WalletNetwork::Mainnet,
            Self::Regtest => WalletNetwork::Regtest,
            _ => WalletNetwork::Testnet,
        }
    }
}

pub trait KernelBlockExt {
    fn convert(self) -> bitcoin::Block;
}

impl KernelBlockExt for KernelBlock {
    fn convert(self) -> bitcoin::Block {
        encode::deserialize(
            &self
                .consensus_encode()
                .expect("block has valid serialization."),
        )
        .expect("block has valid serialization.")
    }
}

pub trait CrateBlockExt {
    fn convert(self) -> KernelBlock;
}

impl CrateBlockExt for bitcoin::Block {
    fn convert(self) -> KernelBlock {
        KernelBlock::try_from(encode::serialize(&self).as_slice()).unwrap()
    }
}

pub trait CrateHeaderExt {
    fn convert(self) -> bitcoinkernel::BlockHeader;
}

impl CrateHeaderExt for bitcoin::block::Header {
    fn convert(self) -> bitcoinkernel::BlockHeader {
        bitcoinkernel::BlockHeader::new(encode::serialize(&self).as_slice()).unwrap()
    }
}

pub trait GetBlockHashExt {
    fn block_hash(&self) -> bitcoin::BlockHash;
}

impl GetBlockHashExt for BlockTreeEntry<'_> {
    fn block_hash(&self) -> bitcoin::BlockHash {
        bitcoin::BlockHash::from_byte_array(self.block_hash().to_bytes())
    }
}
