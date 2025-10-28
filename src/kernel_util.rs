use bitcoin::{block::Checked, consensus::encode, Network, TestnetVersion};
use bitcoinkernel::{BlockTreeEntry, ChainType};
use clap::ValueEnum;
use home::home_dir;
use std::{fs, path::PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl From<BitcoinNetwork> for bitcoin::Network {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => Network::Bitcoin,
            BitcoinNetwork::Testnet => Network::Testnet(TestnetVersion::V3),
            BitcoinNetwork::Signet => Network::Signet,
            BitcoinNetwork::Regtest => Network::Regtest,
        }
    }
}

impl From<BitcoinNetwork> for ChainType {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => ChainType::Mainnet,
            BitcoinNetwork::Testnet => ChainType::Testnet,
            BitcoinNetwork::Signet => ChainType::Signet,
            BitcoinNetwork::Regtest => ChainType::Regtest,
        }
    }
}

pub trait DirnameExt {
    fn data_dir(&self) -> String;
}

impl DirnameExt for String {
    fn data_dir(&self) -> String {
        let path = match self.strip_prefix("~/") {
            Some(rest) => match home_dir() {
                Some(mut home) => {
                    home.push(rest);
                    home
                }
                None => PathBuf::from(rest),
            },
            None => PathBuf::from(self),
        };
        // Create directories if they don't exist
        fs::create_dir_all(&path).unwrap();

        // Get canonical (full) path
        path.canonicalize().unwrap().to_str().unwrap().to_string()
    }
}

pub fn bitcoin_block_to_kernel_block(block: &bitcoin::Block<Checked>) -> bitcoinkernel::Block {
    let ser_block = encode::serialize(block);
    bitcoinkernel::Block::try_from(ser_block.as_slice()).unwrap()
}

pub fn get_block_hash(index: BlockTreeEntry) -> bitcoin::BlockHash {
    bitcoin::BlockHash::from_byte_array(index.block_hash().into())
}
