use crate::xsk_map::SetElementError;
use rustix::io::Errno;
use std::fmt::{Debug, Display, Formatter};

#[derive(Debug)]
pub enum Error {
    Rustix(Errno),
    MarkerAlreadyUsed,
    Wip,
    XskMapError(String),
}

impl std::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rustix(errno) => {
                write!(f, "Rustix error, error number: {errno}")
            }
            Self::MarkerAlreadyUsed => {
                write!(f, "marker already used to create UMEM")
            }
            Error::Wip => {
                write!(f, "wip")
            }
            Error::XskMapError(message) => f.write_str(message),
        }
    }
}

impl From<Errno> for Error {
    fn from(value: Errno) -> Self {
        Error::Rustix(value)
    }
}

impl From<SetElementError> for Error {
    fn from(value: SetElementError) -> Self {
        Error::XskMapError(value.to_string())
    }
}
