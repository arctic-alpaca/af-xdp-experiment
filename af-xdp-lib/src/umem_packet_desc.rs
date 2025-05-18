use crate::umem_packet_desc::sealed::SealedDescriptorImpl;
use rustix::net::xdp::{XdpDesc, XdpDescOptions};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::ptr::NonNull;

pub(crate) mod sealed {
    use std::fmt::Debug;
    use std::ptr::NonNull;

    pub trait SealedDescriptorImpl<'umem, Marker, const CHUNK_SIZE: usize>: Debug
    where
        Marker: Debug,
    {
        type RingType: Copy + Debug;

        fn into_ring_repr(self) -> Self::RingType;
        fn from_ring_repr(ring_repr: Self::RingType, memory: NonNull<u8>) -> Self
        where
            Self: Sized,
        {
            let offset = Self::base_addr(&ring_repr);
            let memory = unsafe { memory.byte_add(offset as usize).cast() };
            Self::from_desc(ring_repr, memory)
        }
        fn from_desc(ring_repr: Self::RingType, memory: NonNull<[u8; CHUNK_SIZE]>) -> Self;
        fn base_addr(desc: &Self::RingType) -> u64;
    }
}

pub trait Descriptor<'umem, Marker, const CHUNK_SIZE: usize>:
    Debug + sealed::SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Descriptor<'umem, Marker, CHUNK_SIZE>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> sealed::SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    type RingType = XdpDesc;

    fn into_ring_repr(self) -> Self::RingType {
        self.descriptor
    }

    fn from_ring_repr(ring_repr: Self::RingType, memory: NonNull<u8>) -> Self {
        let offset = ring_repr.addr & !(CHUNK_SIZE as u64 - 1);
        Self {
            descriptor: ring_repr,
            memory: unsafe { memory.byte_add(offset as usize).cast() },
            lifetime_marker: PhantomData,
            marker: PhantomData,
        }
    }

    fn from_desc(ring_repr: Self::RingType, memory: NonNull<[u8; CHUNK_SIZE]>) -> Self {
        Self {
            descriptor: ring_repr,
            memory,
            lifetime_marker: PhantomData,
            marker: PhantomData,
        }
    }

    fn base_addr(desc: &Self::RingType) -> u64 {
        desc.addr & !(CHUNK_SIZE as u64 - 1)
    }
}

#[derive(Debug)]
pub struct RxTxFrameDescriptor<'umem, Marker, const CHUNK_SIZE: usize>
where
    Marker: Debug,
{
    descriptor: XdpDesc,
    memory: NonNull<[u8; CHUNK_SIZE]>,
    lifetime_marker: PhantomData<&'umem ()>,
    marker: PhantomData<Marker>,
}

impl<'umem, Marker, const CHUNK_SIZE: usize> RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    pub fn memory(&self) -> &[u8; CHUNK_SIZE] {
        unsafe { self.memory.as_ref() }
    }

    pub fn memory_mut(&mut self) -> &mut [u8; CHUNK_SIZE] {
        unsafe { self.memory.as_mut() }
    }

    pub fn options(&self) -> XdpDescOptions {
        self.descriptor.options
    }

    pub fn set_options(&mut self, options: XdpDescOptions) {
        self.descriptor.options = options;
    }

    pub fn data_offset(&self) -> usize {
        (self.descriptor.addr
            - <Self as SealedDescriptorImpl<Marker, CHUNK_SIZE>>::base_addr(&self.descriptor))
            as usize
    }

    pub fn length(&self) -> usize {
        self.descriptor.len as usize
    }

    pub fn set_length(&mut self, length: u32) -> Result<(), ExceedsChunkSize> {
        self.set_addr_and_length(self.data_offset(), length)
    }

    pub fn set_addr(&mut self, offset_from_base_addr: usize) -> Result<(), ExceedsChunkSize> {
        self.set_addr_and_length(offset_from_base_addr, self.descriptor.len)
    }

    pub fn set_addr_and_length(
        &mut self,
        offset_from_base_addr: usize,
        length: u32,
    ) -> Result<(), ExceedsChunkSize> {
        if offset_from_base_addr + length as usize > CHUNK_SIZE {
            return Err(ExceedsChunkSize);
        }
        self.descriptor.addr =
            <Self as SealedDescriptorImpl<Marker, CHUNK_SIZE>>::base_addr(&self.descriptor)
                + offset_from_base_addr as u64;
        self.descriptor.len = length;
        Ok(())
    }

    /// Converts [`RxTxFrameDescriptor`] into [`FillCompFrameDescriptor`] with proper const generics.
    ///
    /// Convenience method until [`From`] can infer const generics.
    pub fn into_fill_comp_descriptor(self) -> FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE> {
        self.into()
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize>
    From<FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    fn from(value: FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>) -> Self {
        let xdp_desc = XdpDesc {
            addr: value.addr,
            len: 0,
            options: XdpDescOptions::empty(),
        };
        RxTxFrameDescriptor::from_desc(xdp_desc, value.memory)
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize> From<RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    fn from(value: RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>) -> Self {
        FillCompFrameDescriptor::from_desc(<RxTxFrameDescriptor<Marker, CHUNK_SIZE> as SealedDescriptorImpl<Marker, CHUNK_SIZE>>::base_addr(&value.descriptor), value.memory)
    }
}

#[derive(Debug)]
pub struct FillCompFrameDescriptor<'umem, Marker, const CHUNK_SIZE: usize>
where
    Marker: Debug,
{
    addr: u64,
    memory: NonNull<[u8; CHUNK_SIZE]>,
    lifetime_marker: PhantomData<&'umem ()>,
    marker: PhantomData<Marker>,
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Descriptor<'umem, Marker, CHUNK_SIZE>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> sealed::SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    type RingType = u64;

    fn into_ring_repr(self) -> Self::RingType {
        self.addr
    }

    fn from_desc(ring_repr: Self::RingType, memory: NonNull<[u8; CHUNK_SIZE]>) -> Self {
        Self {
            addr: ring_repr,
            memory,
            lifetime_marker: PhantomData,
            marker: PhantomData,
        }
    }

    fn base_addr(desc: &Self::RingType) -> u64 {
        desc & !(CHUNK_SIZE as u64 - 1)
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize> FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
where
    Marker: Debug,
{
    /// Converts [`FillCompFrameDescriptor`] into [`RxTxFrameDescriptor`] with proper const generics.
    ///
    /// Convenience method until [`From`] can infer const generics.
    pub fn into_rx_tx_descriptor(self) -> RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE> {
        self.into()
    }
}

#[derive(Debug)]
pub struct ExceedsChunkSize;

impl Display for ExceedsChunkSize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Length and offset combined exceed chunk size.")
    }
}

impl Error for ExceedsChunkSize {}
