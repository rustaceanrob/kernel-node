use bitcoin::{consensus::{deserialize, encode}, hashes::Hash, Network};
use bitcoinkernel::{BlockIndex, ChainType};
use clap::ValueEnum;

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
            BitcoinNetwork::Testnet => Network::Testnet,
            BitcoinNetwork::Signet => Network::Signet,
            BitcoinNetwork::Regtest => Network::Regtest,
        }
    }
}

impl From<BitcoinNetwork> for ChainType {
    fn from(network: BitcoinNetwork) -> Self {
        match network {
            BitcoinNetwork::Mainnet => ChainType::MAINNET,
            BitcoinNetwork::Testnet => ChainType::TESTNET,
            BitcoinNetwork::Signet => ChainType::SIGNET,
            BitcoinNetwork::Regtest => ChainType::REGTEST,
        }
    }
}

pub fn bitcoin_block_to_kernel_block(block: &bitcoin::Block) -> bitcoinkernel::Block {
    let ser_block = encode::serialize(block);
    bitcoinkernel::Block::try_from(ser_block.as_slice()).unwrap()
}

pub fn kernel_unowned_block_to_block(block: bitcoinkernel::UnownedBlock) -> bitcoin::Block {
    let ser_block: Vec<u8> = block.into();
    deserialize(&ser_block).unwrap()
}

pub fn get_block_hash(index: BlockIndex) -> bitcoin::BlockHash {
    bitcoin::BlockHash::from_byte_array(index.block_hash().hash)
}
