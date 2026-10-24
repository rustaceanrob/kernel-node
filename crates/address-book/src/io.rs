use std::{
    fs::File,
    io::{Read, Write},
};

use crate::{Record, Table};

/// Bytes written at the start of every address book file.
const MAGIC: [u8; 4] = *b"ADBK";

/// Extension the the standard file type.
pub trait FileExt {
    /// Write the entire table to a file.
    fn write_table<const B: usize, const S: usize, const W: usize>(
        &mut self,
        table: &Table<B, S, W>,
    ) -> Result<(), std::io::Error>;

    /// Read the table from file.
    fn read_table<const B: usize, const S: usize, const W: usize>(
        &mut self,
    ) -> Result<Table<B, S, W>, std::io::Error>;
}

impl FileExt for File {
    fn write_table<const B: usize, const S: usize, const W: usize>(
        &mut self,
        table: &Table<B, S, W>,
    ) -> Result<(), std::io::Error> {
        let mut unordered_records = Vec::new();
        for bucket in &table.buckets {
            for record in bucket.records.iter().flatten() {
                if record.is_serializable() {
                    unordered_records.push(record.clone());
                }
            }
        }
        self.write_all(&MAGIC)?;
        let len_bytes = unordered_records.len() as u64;
        self.write_all(&len_bytes.to_le_bytes())?;
        for record in unordered_records {
            let bytes = record.serialize();
            self.write_all(&bytes)?;
        }
        self.flush()?;
        self.sync_data()?;
        Ok(())
    }

    fn read_table<const B: usize, const S: usize, const W: usize>(
        &mut self,
    ) -> Result<Table<B, S, W>, std::io::Error> {
        let mut magic_buf = [0u8; 4];
        self.read_exact(&mut magic_buf)?;
        if magic_buf != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid magic bytes",
            ));
        }
        let mut table = Table::new();
        let mut size_buf = [0u8; 8];
        self.read_exact(&mut size_buf)?;
        let size = u64::from_le_bytes(size_buf);
        for _ in 0..size {
            let record = Record::deserialize(self)?;
            table.add(&record);
        }
        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use std::net::{IpAddr, Ipv4Addr};

    use bitcoin::p2p::{address::AddrV2, ServiceFlags};

    use crate::{tests::random_record, Record, Table};

    use super::FileExt;

    #[test]
    fn test_file_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("address.book");
        let mut file = File::create(&path).unwrap();
        let mut table = Table::<128, 16, 16>::new();
        for _ in 0..500 {
            let record = random_record();
            table.add(&record);
        }
        file.write_table(&table).unwrap();
        drop(file);
        let mut file = File::open(&path).unwrap();
        file.read_table::<128, 16, 16>().unwrap();
    }

    #[test]
    fn test_file_skips_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.book");
        let mut table = Table::<128, 16, 16>::new();
        let known = random_record();
        table.add(&known);
        let unknown = Record::new(
            AddrV2::Unknown(42, vec![0xab; 300]),
            8333,
            ServiceFlags::NONE,
            &IpAddr::V4(Ipv4Addr::LOCALHOST),
        );
        table.add(&unknown);
        let mut file = File::create(&path).unwrap();
        file.write_table(&table).unwrap();
        drop(file);
        let mut file = File::open(&path).unwrap();
        let read = file.read_table::<128, 16, 16>().unwrap();
        assert_eq!(read.count(&known), 1);
        assert_eq!(read.count(&unknown), 0);
    }

    #[test]
    fn test_file_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad_magic.book");
        fs::write(&path, [0u8; 12]).unwrap();
        let mut file = File::open(&path).unwrap();
        let err = file.read_table::<128, 16, 16>().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
