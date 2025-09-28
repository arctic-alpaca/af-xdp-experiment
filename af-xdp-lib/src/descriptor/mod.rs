pub mod error;

use crate::descriptor::error::ExceedsChunkSize;
use crate::descriptor::sealed::SealedDescriptorImpl;
use crate::umem::memory::UmemMemory;
use rustix::net::xdp::{XdpDesc, XdpDescOptions};
use std::any::type_name;
use std::fmt::Debug;
use std::marker::PhantomData;

pub(crate) mod sealed {
    use crate::umem::memory::UmemMemory;

    pub trait SealedDescriptorImpl<'umem, Marker, const CHUNK_SIZE: usize> {
        type InRingDescriptorType: Copy;

        fn into_ring_repr(self) -> Self::InRingDescriptorType;

        #[expect(private_interfaces)]
        fn from_ring_repr(ring_repr: Self::InRingDescriptorType, memory: &'umem UmemMemory) -> Self
        where
            Self: Sized,
        {
            let offset = Self::base_addr(&ring_repr);
            let memory = unsafe { memory.memory().byte_add(offset as usize).cast().as_mut() };
            Self::from_desc(ring_repr, memory)
        }

        fn from_desc(
            ring_repr: Self::InRingDescriptorType,
            memory: &'umem mut [u8; CHUNK_SIZE],
        ) -> Self;

        fn base_addr(desc: &Self::InRingDescriptorType) -> u64;
    }
}

pub trait Descriptor<'umem, Marker, const CHUNK_SIZE: usize>:
    SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Descriptor<'umem, Marker, CHUNK_SIZE>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
    type InRingDescriptorType = XdpDesc;

    fn into_ring_repr(self) -> Self::InRingDescriptorType {
        self.descriptor
    }

    #[expect(private_interfaces)]
    fn from_ring_repr(ring_repr: Self::InRingDescriptorType, memory: &'umem UmemMemory) -> Self {
        let offset = ring_repr.addr & !(CHUNK_SIZE as u64 - 1);

        let memory = unsafe { memory.memory().byte_add(offset as usize).cast().as_mut() };
        Self {
            descriptor: ring_repr,
            memory,
            marker: PhantomData,
        }
    }

    fn from_desc(
        ring_repr: Self::InRingDescriptorType,
        memory: &'umem mut [u8; CHUNK_SIZE],
    ) -> Self {
        Self {
            descriptor: ring_repr,
            memory,
            marker: PhantomData,
        }
    }

    fn base_addr(desc: &Self::InRingDescriptorType) -> u64 {
        desc.addr & !(CHUNK_SIZE as u64 - 1)
    }
}

pub struct RxTxFrameDescriptor<'umem, Marker, const CHUNK_SIZE: usize> {
    descriptor: XdpDesc,
    memory: &'umem mut [u8; CHUNK_SIZE],
    marker: PhantomData<fn(Marker)>,
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Debug
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("RxTxFrameDescriptor<{}>", type_name::<Marker>()))
            .field("descriptor", &self.descriptor)
            .field("memory", &self.memory)
            .finish()
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize> RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE> {
    pub fn memory(&self) -> &[u8; CHUNK_SIZE] {
        self.memory
    }

    pub fn memory_mut(&mut self) -> &mut [u8; CHUNK_SIZE] {
        self.memory
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
}
impl<'umem, Marker, const CHUNK_SIZE: usize>
    From<FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>>
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
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

unsafe impl<'umem, Marker, const CHUNK_SIZE: usize> Send
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}
unsafe impl<'umem, Marker, const CHUNK_SIZE: usize> Sync
    for RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}

pub struct FillCompFrameDescriptor<'umem, Marker, const CHUNK_SIZE: usize> {
    addr: u64,
    memory: &'umem mut [u8; CHUNK_SIZE],
    marker: PhantomData<Marker>,
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Debug
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!(
            "FillCompFrameDescriptor<{}>",
            type_name::<Marker>()
        ))
        .field("addr", &self.addr)
        .field("memory", &self.memory)
        .finish()
    }
}

unsafe impl<'umem, Marker, const CHUNK_SIZE: usize> Send
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}

unsafe impl<'umem, Marker, const CHUNK_SIZE: usize> Sync
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> Descriptor<'umem, Marker, CHUNK_SIZE>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
}

impl<'umem, Marker, const CHUNK_SIZE: usize> SealedDescriptorImpl<'umem, Marker, CHUNK_SIZE>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
    type InRingDescriptorType = u64;

    fn into_ring_repr(self) -> Self::InRingDescriptorType {
        self.addr
    }

    fn from_desc(
        ring_repr: Self::InRingDescriptorType,
        memory: &'umem mut [u8; CHUNK_SIZE],
    ) -> Self {
        Self {
            addr: ring_repr,
            memory,
            marker: PhantomData,
        }
    }

    fn base_addr(desc: &Self::InRingDescriptorType) -> u64 {
        desc & !(CHUNK_SIZE as u64 - 1)
    }
}

impl<'umem, Marker, const CHUNK_SIZE: usize> From<RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>>
    for FillCompFrameDescriptor<'umem, Marker, CHUNK_SIZE>
{
    fn from(value: RxTxFrameDescriptor<'umem, Marker, CHUNK_SIZE>) -> Self {
        FillCompFrameDescriptor::from_desc(<RxTxFrameDescriptor<Marker, CHUNK_SIZE> as SealedDescriptorImpl<Marker, CHUNK_SIZE>>::base_addr(&value.descriptor), value.memory)
    }
}
