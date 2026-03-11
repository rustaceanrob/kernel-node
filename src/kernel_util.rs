use bitcoin::{block::Checked, consensus::encode, Network, TestnetVersion};
use bitcoinkernel::{core::BlockHashExt, BlockTreeEntry, ChainType};
use std::{fs, path::PathBuf};

pub trait ChainExt {
    fn chain_type(&self) -> ChainType;
}

impl ChainExt for Network {
    fn chain_type(&self) -> ChainType {
        match self {
            Network::Bitcoin => ChainType::Mainnet,
            Network::Signet => ChainType::Signet,
            Network::Testnet(TestnetVersion::V3) => ChainType::Testnet,
            Network::Testnet(TestnetVersion::V4) => ChainType::Testnet4,
            Network::Regtest => ChainType::Regtest,
            _ => unimplemented!("unsupported network"),
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

pub fn bitcoin_block_to_kernel_block(block: &bitcoin::Block<Checked>) -> bitcoinkernel::Block {
    let ser_block = encode::serialize(block);
    bitcoinkernel::Block::try_from(ser_block.as_slice()).unwrap()
}

pub fn bitcoin_header_to_kernel_header(
    header: &bitcoin::BlockHeader,
) -> bitcoinkernel::BlockHeader {
    let ser_header = encode::serialize(header);
    bitcoinkernel::BlockHeader::new(ser_header.as_slice()).unwrap()
}

pub fn get_block_hash(index: BlockTreeEntry) -> bitcoin::BlockHash {
    bitcoin::BlockHash::from_byte_array(index.block_hash().to_bytes())
}
