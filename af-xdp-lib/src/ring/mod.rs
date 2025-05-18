mod inner;

use crate::error::Error;
use crate::umem_packet_desc::{Descriptor, FillCompFrameDescriptor, RxTxFrameDescriptor};
use crate::xsk_map::XskMapEntry;
use crate::xsk_map::{SetElementError, XskMap};
use inner::InnerRing;
use rustix::io::Errno;
use rustix::mm::munmap;
use rustix::net::xdp::{
    SocketAddrXdp, XDP_PGOFF_RX_RING, XDP_PGOFF_TX_RING, XDP_UMEM_PGOFF_COMPLETION_RING,
    XDP_UMEM_PGOFF_FILL_RING, XdpMmapOffsets, XdpRingFlags, XdpRingOffset,
};
use rustix::net::{RecvFlags, SendFlags, recvfrom, sendto};
use std::ffi::c_void;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use tracing::info;

/// https://github.com/xdp-project/xdp-tools/blob/master/headers/xdp/xsk.h#L32
/// https://github.com/torvalds/linux/blob/master/net/xdp/xsk_queue.h
#[derive(Debug)]
pub struct Ring<
    'umem,
    // Ring type
    RT,
    FrameDescriptor,
    Marker,
    // XSK map
    XM,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
    XM: XskMap,
{
    // Drop the inner ring before the ring memory to avoid dangling pointers.
    ring: InnerRing<'umem, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>,
    // Drop the map entry before the ring memory to ensure the socket is removed from the XSKMAP before the memory is unmapped.
    map_entry: Option<XskMapEntry<XM>>,
    _ring_memory: RingMemory,
    socket: Arc<OwnedFd>,
    ring_type: PhantomData<RT>,
    marker: PhantomData<Marker>,
}

// Safety:
// Ring is not Send because NonNull has no guarantees to make it Send.
// The pointers are never altered, and the pointed to memory/values are safe to exclusively access from other threads.
unsafe impl<'umem, RT, FrameDescriptor, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Send for Ring<'umem, RT, FrameDescriptor, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
    XM: XskMap,
{
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Copy, Clone)]
pub struct Consumer;
#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Copy, Clone)]
pub struct Producer;

pub type RxRing<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Consumer,
    RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    XM,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type TxRing<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Producer,
    RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    XM,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type CompletionRing<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Consumer,
    FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    XM,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type FillRing<'umem, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Producer,
    FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    XM,
    CHUNK_SIZE,
    RING_SIZE,
>;

fn get_offsets(socket_fd: impl AsFd) -> Result<XdpMmapOffsets, Errno> {
    rustix::net::sockopt::xdp_mmap_offsets(socket_fd)
}

impl<Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    RxRing<'_, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    Marker: Debug,
    XM: XskMap,
{
    pub fn new(umem_memory: NonNull<u8>, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        rustix::net::sockopt::set_xdp_rx_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = get_offsets(socket.as_fd())?.rx;
        RxRing::internal_new(offsets, XDP_PGOFF_RX_RING, umem_memory, socket)
    }

    // Completion ring does not need a poke.
    // Tx rings sendmsg to start sending
    // XDP_USE_NEED_WAKEUP
    // Poll wakes RX and TX, sendto wakes TX, recvmsg wakes RX
    // FILL: recvmsg (because it actually wakes the RX ring)
    // TX: sendto
    // RX: recvmsg
    pub fn poke(&self) {
        if let Some(flags) = self.flags() {
            if flags == XdpRingFlags::XDP_RING_NEED_WAKEUP {
                recvfrom::<_, &mut [u8; 0]>(self.socket.as_fd(), &mut [], RecvFlags::DONTWAIT)
                    .unwrap();
            }
        }
    }

    pub(crate) fn register(
        &mut self,
        xsk_map: Arc<Mutex<XM>>,
        xsk_map_index: u32,
    ) -> Result<(), SetElementError> {
        info!("registering in map");
        self.map_entry = Some(XskMapEntry::new(
            xsk_map,
            xsk_map_index,
            self.socket.clone(),
        )?);
        Ok(())
    }
}

impl<Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    TxRing<'_, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    Marker: Debug,
    XM: XskMap,
{
    pub fn new(umem_memory: NonNull<u8>, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        rustix::net::sockopt::set_xdp_tx_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = get_offsets(socket.as_fd())?.tx;
        TxRing::internal_new(offsets, XDP_PGOFF_TX_RING, umem_memory, socket)
    }

    // Completion ring does not need a poke
    // Tx rings sendmsg to start sending
    // XDP_USE_NEED_WAKEUP
    // Poll wakes RX and TX, sendto wakes TX, recvmsg wakes RX
    // FILL: recvmsg (because it actually wakes the RX ring)
    // TX: sendto
    // RX: recvmsg
    pub fn poke(&self) {
        if let Some(flags) = self.flags() {
            if flags == XdpRingFlags::XDP_RING_NEED_WAKEUP {
                let sockaddr_xdp = SocketAddrXdp::new(
                    // Not used in sendmsg for XDP.
                    // https://github.com/torvalds/linux/blob/v6.10/net/xdp/xsk.c#L905-L948
                    rustix::net::xdp::SocketAddrXdpFlags::empty(),
                    // Not used in sendmsg for XDP.
                    // https://github.com/torvalds/linux/blob/v6.10/net/xdp/xsk.c#L905-L948
                    0,
                    // Not used in sendmsg for XDP.
                    // https://github.com/torvalds/linux/blob/v6.10/net/xdp/xsk.c#L905-L948
                    0,
                );
                sendto(self.socket.as_fd(), &[], SendFlags::DONTWAIT, &sockaddr_xdp).unwrap();
            }
        }
    }
}

impl<Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    CompletionRing<'_, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    Marker: Debug,
    XM: XskMap,
{
    pub(crate) fn new(umem_memory: NonNull<u8>, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        rustix::net::sockopt::set_xdp_umem_completion_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = get_offsets(socket.as_fd())?.cr;
        CompletionRing::internal_new(offsets, XDP_UMEM_PGOFF_COMPLETION_RING, umem_memory, socket)
    }
}

impl<Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    FillRing<'_, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    Marker: Debug,
    XM: XskMap,
{
    pub(crate) fn new(umem_memory: NonNull<u8>, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        rustix::net::sockopt::set_xdp_umem_fill_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = get_offsets(socket.as_fd())?.fr;
        FillRing::internal_new(offsets, XDP_UMEM_PGOFF_FILL_RING, umem_memory, socket)
    }

    // Completion ring does not need a poke
    // Tx rings sendmsg to start sending
    // XDP_USE_NEED_WAKEUP
    // Poll wakes RX and TX, sendto wakes TX, recvmsg wakes RX
    // FILL: recvmsg (because it actually wakes the RX ring)
    // TX: sendto
    // RX: recvmsg
    pub fn poke(&self) {
        if let Some(flags) = self.flags() {
            if flags == XdpRingFlags::XDP_RING_NEED_WAKEUP {
                // The wakeup flag in the fill ring means we need to wake up the RX ring:
                // https://github.com/torvalds/linux/commit/77cd0d7b3f257fd0e3096b4fdcff1a7d38e99e10
                // This means we can use recvfrom like in the RX ring.
                recvfrom::<_, &mut [u8; 0]>(self.socket.as_fd(), &mut [], RecvFlags::DONTWAIT)
                    .unwrap();
            }
        }
    }
}

impl<'umem, FrameDescriptor, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, Producer, FrameDescriptor, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
    XM: XskMap,
{
    pub fn push(&mut self, input: FrameDescriptor) -> Result<(), FrameDescriptor> {
        self.ring.push(input)
    }
}

impl<'umem, FrameDescriptor, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, Consumer, FrameDescriptor, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
    XM: XskMap,
{
    pub fn pop(&mut self) -> Option<FrameDescriptor> {
        self.ring.pop()
    }
}

impl<'umem, RT, FrameDescriptor, Marker, XM, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, RT, FrameDescriptor, Marker, XM, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
    XM: XskMap,
{
    fn internal_new(
        offsets: XdpRingOffset,
        mmap_offset: u64,
        umem_memory: NonNull<u8>,
        socket: Arc<OwnedFd>,
    ) -> Result<Ring<'umem, RT, FrameDescriptor, Marker, XM, CHUNK_SIZE, RING_SIZE>, Error> {
        const {
            assert!(RING_SIZE as u32 <= u32::MAX >> 1);
            assert!(RING_SIZE.is_power_of_two());
        }
        info!("Offsets: {offsets:?}");

        let mmap_len = Self::mmap_len(offsets.desc as usize);

        let ring_memory = RingMemory::new(mmap_len, socket.as_fd(), mmap_offset)?;

        let ring = InnerRing::new(
            ring_memory.producer(&offsets)?,
            ring_memory.consumer(&offsets)?,
            ring_memory.descriptors::<FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>(&offsets)?,
            ring_memory.flags(&offsets)?,
            umem_memory,
        );

        Ok(Ring {
            ring,
            _ring_memory: ring_memory,
            socket,
            map_entry: None,
            ring_type: PhantomData,
            marker: PhantomData,
        })
    }

    pub fn flags(&self) -> Option<XdpRingFlags> {
        self.ring.flags()
    }

    pub fn needs_wakeup(&self) -> bool {
        self.ring.needs_wakeup()
    }

    pub fn free_entries(&self) -> u32 {
        self.ring.free_entries()
    }

    pub fn filled_entries(&self) -> u32 {
        self.ring.filled_entries()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.ring.is_full()
    }

    fn mmap_len(descriptors_offset: usize) -> usize {
        descriptors_offset + RING_SIZE * size_of::<FrameDescriptor>()
    }
}

#[derive(Debug)]
pub(crate) struct RingMemory {
    address: NonNull<c_void>,
    size: usize,
}

impl RingMemory {
    pub(crate) fn new(size: usize, socket: BorrowedFd, offset: u64) -> Result<Self, Error> {
        let address = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                size,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED | rustix::mm::MapFlags::POPULATE,
                socket,
                offset,
            )
        }?;
        let address = NonNull::new(address).ok_or(Error::Wip).inspect_err(|_| {
            unsafe { munmap(address, size) }.unwrap();
        })?;
        Ok(Self { address, size })
    }

    pub(crate) fn producer(&self, offsets: &XdpRingOffset) -> Result<NonNull<AtomicU32>, Error> {
        let producer = unsafe { self.address.as_ptr().byte_add(offsets.producer as usize) };
        NonNull::new(producer.cast()).ok_or(Error::Wip)
    }

    pub(crate) fn consumer(&self, offsets: &XdpRingOffset) -> Result<NonNull<AtomicU32>, Error> {
        let consumer = unsafe { self.address.as_ptr().byte_add(offsets.consumer as usize) };
        NonNull::new(consumer.cast()).ok_or(Error::Wip)
    }

    pub(crate) fn flags(&self, offsets: &XdpRingOffset) -> Result<Option<NonNull<u32>>, Error> {
        if let Some(flags) = offsets.flags {
            let flags = unsafe { self.address.as_ptr().byte_add(flags as usize) };
            Ok(Some(NonNull::new(flags).unwrap().cast()))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn descriptors<
        'umem,
        FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE>,
        Marker: Debug,
        const CHUNK_SIZE: usize,
        const RING_SIZE: usize,
    >(
        &self,
        offsets: &XdpRingOffset,
    ) -> Result<NonNull<[FrameDescriptor::RingType; RING_SIZE]>, Error> {
        NonNull::new(unsafe { self.address.as_ptr().byte_add(offsets.desc as usize).cast() })
            .ok_or(Error::Wip)
    }
}

impl Drop for RingMemory {
    fn drop(&mut self) {
        unsafe { munmap(self.address.as_ptr(), self.size) }.unwrap()
    }
}
