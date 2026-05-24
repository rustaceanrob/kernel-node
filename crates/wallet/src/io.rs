use std::{io, path::Path};

pub trait FileExt: Sized {
    type Error: From<io::Error>;
    fn save(&self, path: &Path) -> Result<(), Self::Error>;
    fn load(path: &Path) -> Result<Self, Self::Error>;
}
