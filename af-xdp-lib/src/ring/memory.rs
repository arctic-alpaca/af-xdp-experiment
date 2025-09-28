use crate::descriptor::Descriptor;
use crate::error::Error;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::net::xdp::{XdpRingFlags, XdpRingOffset};
use std::ffi::c_void;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Release};

pub(crate) struct RingMemory<
    'umem,
    Marker,
    FrameDescriptor,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE>,
{
    mmap_address: NonNull<c_void>,
    mmap_size: usize,
    descriptor_memory: NonNull<FrameDescriptor::InRingDescriptorType>,
    producer: NonNull<AtomicU32>,
    consumer: NonNull<AtomicU32>,
    flags: Option<NonNull<u32>>,
}

impl<'umem, Marker, FrameDescriptor, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    RingMemory<'umem, Marker, FrameDescriptor, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE>,
{
    pub(crate) fn new(
        socket: BorrowedFd,
        mmap_offset: u64,
        ring_offsets: XdpRingOffset,
    ) -> Result<Self, Error> {
        let mmap_size = (ring_offsets.desc as usize)
            + RING_SIZE * size_of::<FrameDescriptor::InRingDescriptorType>();

        let mmap_address = unsafe {
            mmap(
                std::ptr::null_mut(),
                mmap_size,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED | MapFlags::POPULATE,
                socket,
                mmap_offset,
            )
        }?;

        let mmap_address = NonNull::new(mmap_address)
            .ok_or(Error::Wip)
            .inspect_err(|_| {
                unsafe { munmap(mmap_address, mmap_size) }.unwrap();
            })?;

        Ok(Self {
            mmap_address,
            mmap_size,
            descriptor_memory: Self::descriptors_memory_ptr(mmap_address, &ring_offsets),
            producer: Self::producer_ptr(mmap_address, &ring_offsets),
            consumer: Self::consumer_ptr(mmap_address, &ring_offsets),
            flags: Self::flags_ptr(mmap_address, &ring_offsets),
        })
    }

    pub fn producer_ptr(
        mmap_address: NonNull<c_void>,
        offsets: &XdpRingOffset,
    ) -> NonNull<AtomicU32> {
        let producer = unsafe { mmap_address.byte_add(offsets.producer as usize) };
        producer.cast()
    }

    pub fn consumer_ptr(
        mmap_address: NonNull<c_void>,
        offsets: &XdpRingOffset,
    ) -> NonNull<AtomicU32> {
        let consumer = unsafe { mmap_address.byte_add(offsets.consumer as usize) };
        consumer.cast()
    }

    pub fn flags_ptr(
        mmap_address: NonNull<c_void>,
        offsets: &XdpRingOffset,
    ) -> Option<NonNull<u32>> {
        if let Some(flags) = offsets.flags {
            let flags = unsafe { mmap_address.byte_add(flags as usize) };
            Some(flags.cast())
        } else {
            None
        }
    }

    fn descriptors_memory_ptr(
        mmap_address: NonNull<c_void>,
        offsets: &XdpRingOffset,
    ) -> NonNull<FrameDescriptor::InRingDescriptorType> {
        let descriptor_memory = unsafe { mmap_address.byte_add(offsets.desc as usize) };
        descriptor_memory.cast()
    }

    /// Returns a value with all bits of the index value set.
    ///
    /// The ring index overflows and the ring index bits are used to cut off the overflow part.
    fn ring_index_bits() -> usize {
        RING_SIZE - 1
    }

    fn producer_ref(&self) -> &AtomicU32 {
        unsafe { self.producer.as_ref() }
    }

    fn consumer_ref(&self) -> &AtomicU32 {
        unsafe { self.consumer.as_ref() }
    }

    pub(crate) fn producer(&self) -> u32 {
        self.producer_ref().load(Acquire)
    }

    pub(crate) fn set_producer(&self, producer: u32) {
        self.producer_ref().store(producer, Release);
    }

    pub(crate) fn consumer(&self) -> u32 {
        self.consumer_ref().load(Acquire)
    }

    pub(crate) fn set_consumer(&self, consumer: u32) {
        self.consumer_ref().store(consumer, Release);
    }

    pub(crate) fn flags(&self) -> Option<XdpRingFlags> {
        // We need to read the value instead of creating a reference to avoid possibly violating Rust aliasing rules.
        // While we hold a non-mutable reference, the kernel might mutate the data.
        let flags = unsafe { self.flags?.read() };
        Some(XdpRingFlags::from_bits_retain(flags))
    }

    pub(crate) unsafe fn read_descriptor(
        &self,
        mut offset: usize,
    ) -> FrameDescriptor::InRingDescriptorType {
        offset &= Self::ring_index_bits();
        let desc_ptr = unsafe { self.descriptor_memory.add(offset) };
        unsafe { desc_ptr.read() }
    }

    pub(crate) unsafe fn write_descriptor(
        &self,
        mut offset: usize,
        desc: FrameDescriptor::InRingDescriptorType,
    ) {
        offset &= Self::ring_index_bits();
        let desc_ptr = unsafe { self.descriptor_memory.add(offset) };
        unsafe { desc_ptr.write(desc) }
    }
}

impl<'umem, Marker, FrameDescriptor, const CHUNK_SIZE: usize, const RING_SIZE: usize> Drop
    for RingMemory<'umem, Marker, FrameDescriptor, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE>,
{
    fn drop(&mut self) {
        unsafe { munmap(self.mmap_address.as_ptr(), self.mmap_size) }.unwrap()
    }
}
