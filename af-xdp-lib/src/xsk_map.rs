use crate::ring::{CompletionRing, FillRing, RxRing, TxRing};
use crate::umem::{DeviceId, QueueId, Umem};
use aya::maps::MapData;
use std::borrow::BorrowMut;
use std::fmt::{Debug, Display};
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Mutex;
use tracing::{error, info};

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
    fn unset_element(&mut self, index: u32) -> Result<(), UnsetElementError>;

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

#[derive(Debug)]
pub struct UnsetElementError(String);

impl Display for UnsetElementError {
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

    fn unset_element(&mut self, index: u32) -> Result<(), UnsetElementError> {
        self.unset(index)
            .map_err(|error| UnsetElementError(error.to_string()))
    }

    fn max_entries(&self) -> u32 {
        self.len()
    }
}

pub(crate) struct XskMapEntry<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize>
where
    XM: XskMap,
    Marker: 'static,
{
    xsk_map: &'xsk XskMapStorage<'umem, XM, Marker, CHUNK_SIZE>,
    xsk_lifetime: PhantomData<&'xsk ()>,
    index: u32,
}

impl<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize>
    XskMapEntry<'umem, 'xsk, XM, Marker, CHUNK_SIZE>
where
    XM: XskMap,
{
    pub(crate) fn new(
        xsk_map: &'xsk XskMapStorage<'umem, XM, Marker, CHUNK_SIZE>,
        index: u32,
        socket: impl AsRawFd,
    ) -> Result<Self, SetElementError> {
        info!("registering socket at index: {}", index);
        xsk_map.register(socket, index)?;
        Ok(Self {
            xsk_map,
            xsk_lifetime: PhantomData,
            index,
        })
    }
}

impl<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize> Drop
    for XskMapEntry<'umem, 'xsk, XM, Marker, CHUNK_SIZE>
where
    XM: XskMap,
    Marker: 'static,
{
    fn drop(&mut self) {
        if let Err(error) = self.xsk_map.deregister(self.index) {
            error!(
                "failed to deregister socket at index {}: {}",
                self.index, error
            );
        }
    }
}

pub struct XskMapStorage<'umem, XM, Marker, const CHUNK_SIZE: usize>
where
    XM: XskMap,
    Marker: 'static,
{
    xsk_map: Mutex<XM>,
    net_device_id: DeviceId,
    umem: &'umem Umem<Marker, CHUNK_SIZE>,
}

impl<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize> XskMapStorage<'umem, XM, Marker, CHUNK_SIZE>
where
    XM: XskMap,
{
    pub fn new(
        xsk_map: XM,
        net_device_id: DeviceId,
        umem: &'umem Umem<Marker, CHUNK_SIZE>,
    ) -> Self {
        Self {
            xsk_map: Mutex::new(xsk_map),
            net_device_id,
            umem,
        }
    }

    pub(crate) fn register(&self, socket: impl AsRawFd, index: u32) -> Result<(), SetElementError> {
        self.xsk_map.lock().unwrap().set_element(socket, index)
    }

    pub(crate) fn deregister(&self, index: u32) -> Result<(), UnsetElementError> {
        self.xsk_map.lock().unwrap().unset_element(index)
    }

    pub fn into_inner(self) -> XM {
        // If there is no reference to this XskMap, then there is also no other reference to the Arc.
        self.xsk_map.into_inner().unwrap()
    }

    pub fn rings<const RING_SIZE: usize>(
        &'xsk self,
        queue_id: QueueId,
        map_index: u32,
    ) -> Rings<'umem, 'xsk, Marker, XM, CHUNK_SIZE, RING_SIZE> {
        info!("rings");

        let socket = self.umem.xsk_map_socket();

        let rx_ring = RxRing::new(self.umem.memory(), socket.clone()).unwrap();
        let tx_ring = TxRing::new(self.umem.memory(), socket.clone()).unwrap();
        let fill_ring = FillRing::new(self.umem.memory(), socket.clone()).unwrap();
        let completion_ring = CompletionRing::new(self.umem.memory(), socket.clone()).unwrap();

        if self
            .umem
            .bind_socket(socket.clone(), self.net_device_id, queue_id)
            .is_ok()
        {
            let xsk_map_entry = XskMapEntry::new(self, map_index, socket.as_fd()).unwrap();
            Rings::Four(FillCompRxTxRings {
                _xsk_map_entry: xsk_map_entry,
                fill_ring,
                completion_ring,
                rx_ring,
                tx_ring,
            })
        } else {
            let socket = self.umem.xsk_map_socket();

            let rx_ring = RxRing::new(self.umem.memory(), socket.clone()).unwrap();
            let tx_ring = TxRing::new(self.umem.memory(), socket.clone()).unwrap();

            self.umem
                .bind_socket(socket.clone(), self.net_device_id, queue_id)
                .unwrap();

            let xsk_map_entry = XskMapEntry::new(self, map_index, socket.as_fd()).unwrap();
            Rings::Two(RxTxRings {
                _xsk_map_entry: xsk_map_entry,
                rx_ring,
                tx_ring,
            })
        }
    }
}

pub enum Rings<'umem, 'xsk, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
where
    XM: XskMap,
    Marker: 'static,
{
    Two(RxTxRings<'umem, 'xsk, Marker, XM, CHUNK_SIZE, RING_SIZE>),
    Four(FillCompRxTxRings<'umem, 'xsk, Marker, XM, CHUNK_SIZE, RING_SIZE>),
}

pub struct RxTxRings<'umem, 'xsk, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
where
    XM: XskMap,
    Marker: 'static,
{
    // Drop the map entry before the rings to ensure the socket is removed from the XSKMAP before the memory is unmapped.
    _xsk_map_entry: XskMapEntry<'umem, 'xsk, XM, Marker, CHUNK_SIZE>,
    rx_ring: RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    tx_ring: TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
}

impl<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    RxTxRings<'umem, 'xsk, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    XM: XskMap,
{
    pub fn rx_ring(&mut self) -> &mut RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.rx_ring
    }

    pub fn tx_ring(&mut self) -> &mut TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.tx_ring
    }

    pub fn rings(
        &mut self,
    ) -> (
        &mut RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
        &mut TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    ) {
        (&mut self.rx_ring, &mut self.tx_ring)
    }
}

pub struct FillCompRxTxRings<
    'umem,
    'xsk,
    Marker,
    XM,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> where
    XM: XskMap,
    Marker: 'static,
{
    // Drop the map entry before the rings to ensure the socket is removed from the XSKMAP before the memory is unmapped.
    _xsk_map_entry: XskMapEntry<'umem, 'xsk, XM, Marker, CHUNK_SIZE>,
    fill_ring: FillRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    completion_ring: CompletionRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    rx_ring: RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    tx_ring: TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
}

impl<'umem, 'xsk, XM, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    FillCompRxTxRings<'umem, 'xsk, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    XM: XskMap,
{
    pub fn fill_ring(&mut self) -> &mut FillRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.fill_ring
    }

    pub fn completion_ring(&mut self) -> &mut CompletionRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.completion_ring
    }

    pub fn rx_ring(&mut self) -> &mut RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.rx_ring
    }

    pub fn tx_ring(&mut self) -> &mut TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE> {
        &mut self.tx_ring
    }

    pub fn rings(
        &mut self,
    ) -> (
        &mut FillRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
        &mut CompletionRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
        &mut RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
        &mut TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>,
    ) {
        (
            &mut self.fill_ring,
            &mut self.completion_ring,
            &mut self.rx_ring,
            &mut self.tx_ring,
        )
    }
}
