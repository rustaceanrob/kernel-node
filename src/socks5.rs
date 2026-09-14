use bitcoin_hashes::sha3_256;

const SALT: &[u8] = b".onion checksum";
const TOR_VERSION: u8 = 0x03;
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnionAddress([u8; 32]);

impl OnionAddress {
    pub const fn from_pubkey(key: [u8; 32]) -> Self {
        Self(key)
    }
}

impl core::fmt::Display for OnionAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let buf = pubkey_to_service(&self.0);
        // Every byte comes from ALPHABET (ASCII) or the literal b".onion".
        f.write_str(std::str::from_utf8(&buf).expect("ASCII"))
    }
}

#[inline(always)]
fn pubkey_to_service(ed25519: &[u8; 32]) -> [u8; 62] {
    // SHA3(".onion checksum" + public key + version)
    let mut cs_input = [0u8; 48];
    cs_input[..15].copy_from_slice(SALT);
    cs_input[15..47].copy_from_slice(ed25519);
    cs_input[47] = TOR_VERSION;
    let cs = sha3_256::hash(&cs_input).to_byte_array();
    // Onion address = public key + 2 byte checksum + version
    let mut payload = [0u8; 35];
    payload[..32].copy_from_slice(ed25519);
    payload[32] = cs[0];
    payload[33] = cs[1];
    payload[34] = TOR_VERSION;
    let encoded = base32_encode_35(&payload);
    let mut out = [0u8; 62];
    out[..56].copy_from_slice(&encoded);
    out[56..].copy_from_slice(b".onion");
    out
}

#[inline(always)]
fn base32_encode_35(data: &[u8; 35]) -> [u8; 56] {
    // 35 * 8 == 280 is divisible by 5, so no trailing bits remain.
    let mut out = [0u8; 56];
    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;
    let mut i = 0;
    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            out[i] = ALPHABET[((buffer >> bits_left) & 0x1f) as usize];
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::pubkey_to_service;
    use bitcoin::hex::FromHex;

    #[test]
    fn public_key_to_service() {
        let hex = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let hsid = <[u8; 32]>::from_hex(hex).unwrap();
        let service = pubkey_to_service(&hsid);
        assert_eq!(
            b"25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion",
            &service
        );
    }
}
