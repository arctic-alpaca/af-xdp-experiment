mod memory;

use crate::descriptor::{Descriptor, FillCompFrameDescriptor, RxTxFrameDescriptor};
use crate::error::Error;
use crate::ring::memory::RingMemory;
use crate::umem::memory::UmemMemory;
use rustix::net::sockopt::{
    set_xdp_rx_ring_size, set_xdp_tx_ring_size, set_xdp_umem_completion_ring_size,
    set_xdp_umem_fill_ring_size, xdp_mmap_offsets, xdp_options, xdp_statistics,
};
use rustix::net::xdp::{
    SocketAddrXdp, SocketAddrXdpFlags, XDP_PGOFF_RX_RING, XDP_PGOFF_TX_RING,
    XDP_UMEM_PGOFF_COMPLETION_RING, XDP_UMEM_PGOFF_FILL_RING, XdpOptionsFlags, XdpRingFlags,
    XdpRingOffset, XdpStatistics,
};
use rustix::net::{RecvFlags, SendFlags, recvfrom, sendto};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use tracing::{info, trace, warn};

/// https://github.com/xdp-project/xdp-tools/blob/master/headers/xdp/xsk.h#L32
/// https://github.com/torvalds/linux/blob/master/net/xdp/xsk_queue.h
pub struct Ring<
    'umem,
    RingType,
    FrameDescriptor,
    Marker,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
    ring_memory: RingMemory<'umem, Marker, FrameDescriptor, CHUNK_SIZE, RING_SIZE>,
    umem_memory: &'umem UmemMemory,
    socket: Arc<OwnedFd>,
    ring_type: PhantomData<RingType>,
    marker: PhantomData<Marker>,
}

// Safety:
// Ring is not Send because NonNull has no guarantees to make it Send.
// The pointers are never altered, and the pointed to memory/values are safe to exclusively access from other threads.
unsafe impl<
    'umem,
    RingType,
    FrameDescriptor,
    Marker,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> Send for Ring<'umem, RingType, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
}

unsafe impl<
    'umem,
    RingType,
    FrameDescriptor,
    Marker,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> Sync for Ring<'umem, RingType, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Copy, Clone)]
pub struct Consumer;
#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Copy, Clone)]
pub struct Producer;

pub type RxRing<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Consumer,
    RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type TxRing<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Producer,
    RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type CompletionRing<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Consumer,
    FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    CHUNK_SIZE,
    RING_SIZE,
>;
pub type FillRing<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize> = Ring<
    'umem,
    Producer,
    FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>,
    Marker,
    CHUNK_SIZE,
    RING_SIZE,
>;

impl<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    RxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>
{
    pub(crate) fn new(umem_memory: &'umem UmemMemory, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        set_xdp_rx_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = xdp_mmap_offsets(socket.as_fd())?.rx;
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
        if let Some(flags) = self.flags()
            && flags == XdpRingFlags::XDP_RING_NEED_WAKEUP
        {
            recvfrom::<_, &mut [u8; 0]>(self.socket.as_fd(), &mut [], RecvFlags::DONTWAIT).unwrap();
        }
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    TxRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>
{
    pub(crate) fn new(umem_memory: &'umem UmemMemory, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        set_xdp_tx_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = xdp_mmap_offsets(socket.as_fd())?.tx;
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
        if let Some(flags) = self.flags()
            && flags == XdpRingFlags::XDP_RING_NEED_WAKEUP
        {
            let sockaddr_xdp = SocketAddrXdp::new(
                // Not used in sendmsg for XDP.
                // https://github.com/torvalds/linux/blob/v6.10/net/xdp/xsk.c#L905-L948
                SocketAddrXdpFlags::empty(),
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

impl<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    CompletionRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>
{
    pub(crate) fn new(umem_memory: &'umem UmemMemory, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        set_xdp_umem_completion_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = xdp_mmap_offsets(socket.as_fd())?.cr;
        CompletionRing::internal_new(offsets, XDP_UMEM_PGOFF_COMPLETION_RING, umem_memory, socket)
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    FillRing<'umem, Marker, CHUNK_SIZE, RING_SIZE>
{
    pub(crate) fn new(umem_memory: &'umem UmemMemory, socket: Arc<OwnedFd>) -> Result<Self, Error> {
        set_xdp_umem_fill_ring_size(socket.as_fd(), RING_SIZE as u32)?;
        let offsets = xdp_mmap_offsets(socket.as_fd())?.fr;
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
        if let Some(flags) = self.flags()
            && flags == XdpRingFlags::XDP_RING_NEED_WAKEUP
        {
            // The wakeup flag in the fill ring means we need to wake up the RX ring:
            // https://github.com/torvalds/linux/commit/77cd0d7b3f257fd0e3096b4fdcff1a7d38e99e10
            // This means we can use recvfrom like in the RX ring.
            recvfrom::<_, &mut [u8; 0]>(self.socket.as_fd(), &mut [], RecvFlags::DONTWAIT).unwrap();
        }
    }
}

impl<'umem, FrameDescriptor, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, Producer, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
    pub fn push(&mut self, input: FrameDescriptor) -> Result<(), FrameDescriptor> {
        trace!("Pushing {:?}.", &input);

        if !self.is_full() {
            let producer = self.ring_memory.producer();

            unsafe {
                self.ring_memory
                    .write_descriptor(producer as usize, input.into_ring_repr())
            };

            self.ring_memory.set_producer(producer.wrapping_add(1));

            Ok(())
        } else {
            warn!("Pushing failed, ring full: {:?}", &input);
            Err(input)
        }
    }
}

impl<'umem, FrameDescriptor, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, Consumer, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
    pub fn pop(&mut self) -> Option<FrameDescriptor> {
        trace!("Popping.");
        if !self.is_empty() {
            let consumer = self.ring_memory.consumer();

            let desc = FrameDescriptor::from_ring_repr(
                unsafe { self.ring_memory.read_descriptor(consumer as usize) },
                self.umem_memory,
            );

            self.ring_memory.set_consumer(consumer.wrapping_add(1));

            Some(desc)
        } else {
            warn!("Popping failed, the ring is empty.");
            None
        }
    }
}

impl<'umem, RingType, FrameDescriptor, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    Ring<'umem, RingType, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
{
    fn internal_new(
        offsets: XdpRingOffset,
        mmap_offset: u64,
        umem_memory: &'umem UmemMemory,
        socket: Arc<OwnedFd>,
    ) -> Result<Ring<'umem, RingType, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>, Error> {
        const {
            assert!(
                RING_SIZE as u32 <= u32::MAX >> 1,
                "RING_SIZE may not be larger than u32::MAX >> 1 to allow overflow"
            );
            assert!(
                RING_SIZE.is_power_of_two(),
                "RING_SIZE must be a power of two"
            );
        }

        info!("Offsets: {offsets:?}");

        let ring_memory = RingMemory::new(socket.as_fd(), mmap_offset, offsets)?;

        Ok(Ring {
            ring_memory,
            umem_memory,
            socket,
            ring_type: PhantomData,
            marker: PhantomData,
        })
    }

    pub fn statistics(&self) -> Result<XdpStatistics, Error> {
        Ok(xdp_statistics(&self.socket)?)
    }

    pub fn options(&self) -> Result<XdpOptionsFlags, Error> {
        Ok(xdp_options(&self.socket)?)
    }

    pub fn is_zero_copy(&self) -> Result<bool, Error> {
        let option_flags = xdp_options(&self.socket)?;
        Ok(option_flags.contains(XdpOptionsFlags::XDP_OPTIONS_ZEROCOPY))
    }

    pub fn flags(&self) -> Option<XdpRingFlags> {
        self.ring_memory.flags()
    }

    pub fn needs_wakeup(&self) -> bool {
        if let Some(flags) = self.flags() {
            flags.contains(XdpRingFlags::XDP_RING_NEED_WAKEUP)
        } else {
            false
        }
    }

    pub fn free_entries(&self) -> u32 {
        self.ring_memory
            .consumer()
            .wrapping_add(RING_SIZE as u32)
            .wrapping_sub(self.ring_memory.producer())
    }

    pub fn filled_entries(&self) -> u32 {
        self.ring_memory
            .producer()
            .wrapping_sub(self.ring_memory.consumer())
    }

    pub fn is_empty(&self) -> bool {
        self.ring_memory.producer() == self.ring_memory.consumer()
    }

    pub fn is_full(&self) -> bool {
        self.filled_entries() == RING_SIZE as u32
    }
}
