use aya::maps::MapData;
use std::borrow::BorrowMut;
use std::fmt::Display;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use tracing::info;

// https://docs.kernel.org/bpf/map_xskmap.html
pub trait XskMap {
    // BPF_NOEXIST
    fn set_element(&mut self, socket: impl AsRawFd, index: u32) -> Result<(), SetElementError>;
    // BPF_EXIST
    fn update_element(
        &mut self,
        socket: impl AsRawFd,
        index: u32,
    ) -> Result<(), UpdateElementError>;
    fn unset_element(&mut self, index: u32) -> Result<(), ()>;

    fn max_entries(&self) -> u32;
}

#[derive(Debug)]
pub struct SetElementError(String);

impl Display for SetElementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
#[derive(Debug)]
pub struct UpdateElementError(String);

impl Display for UpdateElementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<T> XskMap for aya::maps::XskMap<T>
where
    T: BorrowMut<MapData>,
{
    fn set_element(&mut self, socket: impl AsRawFd, index: u32) -> Result<(), SetElementError> {
        const BPF_FLAG_NO_EXIST: u64 = 1;
        self.set(index, socket, BPF_FLAG_NO_EXIST)
            .map_err(|error| SetElementError(error.to_string()))
    }

    fn update_element(
        &mut self,
        socket: impl AsRawFd,
        index: u32,
    ) -> Result<(), UpdateElementError> {
        const BPF_FLAG_EXIST: u64 = 2;
        self.set(index, socket, BPF_FLAG_EXIST)
            .map_err(|error| UpdateElementError(error.to_string()))
    }

    fn unset_element(&mut self, index: u32) -> Result<(), ()> {
        self.unset(index).map_err(|_| ())
    }

    fn max_entries(&self) -> u32 {
        self.len()
    }
}

#[derive(Debug)]
pub(crate) struct XskMapEntry<XM>
where
    XM: XskMap,
{
    xsk_map: Arc<Mutex<XM>>,
    index: u32,
}

impl<XM> XskMapEntry<XM>
where
    XM: XskMap,
{
    pub(crate) fn new(
        xsk_map: Arc<Mutex<XM>>,
        index: u32,
        socket: impl AsRawFd,
    ) -> Result<Self, SetElementError> {
        info!("registering socket at index: {}", index);
        xsk_map.lock().unwrap().set_element(socket, index)?;
        Ok(Self { xsk_map, index })
    }
}

impl<XM> Drop for XskMapEntry<XM>
where
    XM: XskMap,
{
    fn drop(&mut self) {
        self.xsk_map
            .lock()
            .unwrap()
            .unset_element(self.index)
            .unwrap()
    }
}
