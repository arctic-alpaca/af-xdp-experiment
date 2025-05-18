use crate::error::Error;
use crate::ring::{CompletionRing, FillRing, RxRing, TxRing};
use crate::umem_packet_desc::FillCompFrameDescriptor;
use crate::umem_packet_desc::sealed::SealedDescriptorImpl;
use crate::xsk_map::XskMap;
use rustix::param::page_size;
use std::alloc;
use std::alloc::{GlobalAlloc, Layout, handle_alloc_error};
use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tracing::info;

static USED_MARKERS: LazyLock<Mutex<HashSet<TypeId>>> = LazyLock::new(Default::default);

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u32);

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub struct QueueId(pub u32);

// Subtract 256 bytes reserved for driver https://www.spinics.net/lists/xdp-newbies/msg01479.html
pub const DRIVER_HEADROOM: usize = 256;

#[derive(Debug)]
pub struct Umem<Marker, XM, const CHUNK_SIZE: usize>
where
    Marker: Debug + 'static,
    XM: XskMap,
{
    memory: UmemMemory<CHUNK_SIZE>,
    net_dev_xsk_map_mapping: Mutex<HashMap<DeviceId, Arc<Mutex<XM>>>>,
    descriptors_given_out: AtomicBool,
    initial_rings_given_out: AtomicBool,
    chunk_amount: usize,
    socket: Arc<OwnedFd>,
    marker: PhantomData<Marker>,
}

impl<Marker, XM, const CHUNK_SIZE: usize> Umem<Marker, XM, CHUNK_SIZE>
where
    Marker: Debug + 'static,
    XM: XskMap,
{
    pub fn add_net_dev(&self, net_dev_id: DeviceId, xsk_map: XM) {
        if self
            .net_dev_xsk_map_mapping
            .lock()
            .unwrap()
            .insert(net_dev_id, Arc::new(Mutex::new(xsk_map)))
            .is_some()
        {
            panic!("Device map already exists");
        };
    }

    fn new_internal(headroom: u32, chunk_number: usize) -> Result<Self, Error> {
        info!("Allocate memory.");

        let memory = UmemMemory::new(chunk_number);

        let socket = rustix::net::socket_with(
            rustix::net::AddressFamily::XDP,
            rustix::net::SocketType::RAW,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )?;

        let socket = Arc::new(socket);

        // Initialize umem as early as possible to rely on drop for cleanup.
        let umem = Self {
            memory,

            net_dev_xsk_map_mapping: Default::default(),
            descriptors_given_out: AtomicBool::new(false),
            initial_rings_given_out: AtomicBool::new(false),
            socket: socket.clone(),
            chunk_amount: chunk_number,
            marker: PhantomData,
        };

        info!("Registering UMEM.");
        // Register umem.
        let umem_reg: rustix::net::xdp::XdpUmemReg = rustix::net::xdp::XdpUmemReg {
            addr: umem.memory.memory.as_ptr() as u64,
            len: umem.memory.size() as u64,
            chunk_size: CHUNK_SIZE as u32,
            headroom,
            flags: rustix::net::xdp::XdpUmemRegFlags::empty(),
            tx_metadata_len: 0,
        };

        // The first socket in socket_data_input is used to register UMEM.
        rustix::net::sockopt::set_xdp_umem_reg(socket.as_fd(), umem_reg)?;

        Ok(umem)
    }

    // Ensures the marker is removed if creation isn't successful.
    pub fn new(headroom: u32, chunk_number: usize) -> Result<Self, Error> {
        if !USED_MARKERS.lock().unwrap().insert(TypeId::of::<Marker>()) {
            return Err(Error::MarkerAlreadyUsed);
        }
        Self::new_internal(headroom, chunk_number).inspect_err(|_| {
            USED_MARKERS.lock().unwrap().remove(&TypeId::of::<Marker>());
        })
    }

    pub fn descriptors(&self) -> Option<Vec<FillCompFrameDescriptor<Marker, CHUNK_SIZE>>> {
        if self.descriptors_given_out.swap(true, Ordering::AcqRel) {
            return None;
        }

        Some(
            (0..self.chunk_amount)
                .map(|chunk_index| {
                    let addr = (chunk_index * CHUNK_SIZE) as u64;
                    FillCompFrameDescriptor::from_ring_repr(addr, self.memory.memory)
                })
                .rev()
                .collect(),
        )
    }

    pub fn rx_tx_only<const RING_SIZE: usize>(
        &self,
        net_device_id: DeviceId,
        queue_id: QueueId,
        map_index: u32,
    ) -> Result<RxTXRings<Marker, XM, CHUNK_SIZE, RING_SIZE>, Error> {
        if !self.initial_rings_given_out.load(Ordering::Acquire) {
            return Err(Error::Wip);
        }
        info!("rx tx rings only");

        let socket = self.ring_socket();

        let device_xsk_map = self
            .net_dev_xsk_map_mapping
            .lock()
            .unwrap()
            .get(&net_device_id)
            .cloned()
            .unwrap();

        let mut rx_ring = RxRing::new(self.memory.memory, socket.clone()).unwrap();
        let tx_ring = TxRing::new(self.memory.memory, socket.clone()).unwrap();

        self.bind_socket(socket, net_device_id, queue_id);

        rx_ring.register(device_xsk_map, map_index)?;

        Ok(RxTXRings { rx_ring, tx_ring })
    }

    pub fn rings<const RING_SIZE: usize>(
        &self,
        net_device_id: DeviceId,
        queue_id: QueueId,
        map_index: u32,
    ) -> Result<Rings<Marker, XM, CHUNK_SIZE, RING_SIZE>, Error> {
        info!("rings");

        let socket = self.ring_socket();

        let device_xsk_map = self
            .net_dev_xsk_map_mapping
            .lock()
            .unwrap()
            .get(&net_device_id)
            .cloned()
            .unwrap();

        let fill_ring = FillRing::new(self.memory.memory, socket.clone()).unwrap();
        let completion_ring = CompletionRing::new(self.memory.memory, socket.clone()).unwrap();

        let mut rx_ring = RxRing::new(self.memory.memory, socket.clone()).unwrap();
        let tx_ring = TxRing::new(self.memory.memory, socket.clone()).unwrap();

        self.bind_socket(socket, net_device_id, queue_id);
        rx_ring.register(device_xsk_map, map_index)?;

        Ok(Rings {
            fill_ring,
            completion_ring,
            rx_ring,
            tx_ring,
        })
    }

    fn bind_socket(&self, socket: Arc<OwnedFd>, net_device_id: DeviceId, queue_id: QueueId) {
        if self.initial_rings_given_out.swap(true, Ordering::AcqRel) {
            let sockaddr_xdp = rustix::net::xdp::SocketAddrXdpWithSharedUmem {
                addr: rustix::net::xdp::SocketAddrXdp::new(
                    rustix::net::xdp::SocketAddrXdpFlags::XDP_SHARED_UMEM,
                    net_device_id.0,
                    queue_id.0,
                ),
                shared_umem_fd: self.socket.as_fd(),
            };
            rustix::net::bind(socket.as_fd(), &sockaddr_xdp).unwrap()
        } else {
            let sockaddr_xdp = rustix::net::xdp::SocketAddrXdp::new(
                rustix::net::xdp::SocketAddrXdpFlags::XDP_USE_NEED_WAKEUP,
                net_device_id.0,
                queue_id.0,
            );
            rustix::net::bind(socket.as_fd(), &sockaddr_xdp).unwrap()
        }
    }

    fn ring_socket(&self) -> Arc<OwnedFd> {
        if self.initial_rings_given_out.load(Ordering::Acquire) {
            let socket = rustix::net::socket_with(
                rustix::net::AddressFamily::XDP,
                rustix::net::SocketType::RAW,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            Arc::new(socket)
        } else {
            self.socket.clone()
        }
    }
}

// The Umem can only be dropped when the rings are all dropped; this means no ring remains in the maps.
// The RX ring deregisters the map entry on drop.
impl<Marker, XM, const CHUNK_SIZE: usize> Drop for Umem<Marker, XM, CHUNK_SIZE>
where
    Marker: Debug + 'static,
    XM: XskMap,
{
    fn drop(&mut self) {
        USED_MARKERS.lock().unwrap().remove(&TypeId::of::<Marker>());
    }
}

#[derive(Debug)]
struct UmemMemory<const CHUNK_SIZE: usize> {
    memory: NonNull<u8>,
    chunk_number: usize,
}
impl<const CHUNK_SIZE: usize> UmemMemory<CHUNK_SIZE> {
    fn new(chunk_number: usize) -> Self {
        let page_size = page_size();
        let len = Self::size_internal(chunk_number);
        let layout = Layout::from_size_align(len, page_size).unwrap();
        let umem_region = unsafe { alloc::System.alloc_zeroed(layout) };
        if umem_region.is_null() {
            handle_alloc_error(layout);
        }
        let memory = NonNull::new(umem_region).expect("umem region checked not to be null");

        Self {
            memory,
            chunk_number,
        }
    }

    fn size(&self) -> usize {
        Self::size_internal(self.chunk_number)
    }

    fn size_internal(chunk_number: usize) -> usize {
        chunk_number * CHUNK_SIZE
    }
}

impl<const CHUNK_SIZE: usize> Drop for UmemMemory<CHUNK_SIZE> {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.size(), page_size())
            .expect("Size and page size should have changed since new()");
        unsafe { alloc::System.dealloc(self.memory.as_ptr(), layout) };
    }
}

#[derive(Debug)]
pub struct Rings<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
where
    Marker: Debug,
    XM: XskMap,
{
    pub fill_ring: FillRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
    pub completion_ring: CompletionRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
    pub rx_ring: RxRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
    pub tx_ring: TxRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
}

#[derive(Debug)]
pub struct RxTXRings<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
where
    Marker: Debug,
    XM: XskMap,
{
    pub rx_ring: RxRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
    pub tx_ring: TxRing<'umem, Marker, XM, CHUNK_SIZE, RING_SIZE>,
}
