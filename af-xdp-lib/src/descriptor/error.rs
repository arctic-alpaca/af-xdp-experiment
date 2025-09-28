use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct ExceedsChunkSize;

impl Display for ExceedsChunkSize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Length and offset combined exceed chunk size.")
    }
}

impl Error for ExceedsChunkSize {}
