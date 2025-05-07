/// SQL block header storage.
pub mod headers;
/// SQL peer storage.
pub mod peers;
pub mod blocks;
pub mod filters;

pub(crate) const DEFAULT_CWD: &str = ".";
pub(crate) const DATA_DIR: &str = "data";
