use crate::umem_packet_desc::Descriptor;
use rustix::net::xdp::XdpRingFlags;
use std::fmt::Debug;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Release};
use tracing::{trace, warn};

#[derive(Debug)]
pub(crate) struct InnerRing<
    'umem,
    FrameDescriptor,
    Marker,
    const CHUNK_SIZE: usize,
    const RING_SIZE: usize,
> where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
{
    producer: NonNull<AtomicU32>,
    consumer: NonNull<AtomicU32>,
    pub(crate) ring_region_ptr: NonNull<[FrameDescriptor::RingType; RING_SIZE]>,
    flags: Option<NonNull<u32>>,
    umem_memory: NonNull<u8>,
}

impl<'umem, FrameDescriptor, Marker, const CHUNK_SIZE: usize, const RING_SIZE: usize>
    InnerRing<'umem, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE>
where
    FrameDescriptor: Descriptor<'umem, Marker, CHUNK_SIZE> + Debug,
    Marker: Debug,
{
    pub(crate) fn new(
        producer: NonNull<AtomicU32>,
        consumer: NonNull<AtomicU32>,
        ring_region_ptr: NonNull<[FrameDescriptor::RingType; RING_SIZE]>,
        flags: Option<NonNull<u32>>,
        umem_memory: NonNull<u8>,
    ) -> InnerRing<'umem, FrameDescriptor, Marker, CHUNK_SIZE, RING_SIZE> {
        const {
            assert!(RING_SIZE as u32 <= u32::MAX >> 1);
            assert!(RING_SIZE.is_power_of_two());
        }

        Self {
            producer,
            consumer,
            ring_region_ptr,
            flags,
            umem_memory,
        }
    }

    fn producer_ref(&self) -> &AtomicU32 {
        unsafe { self.producer.as_ref() }
    }

    fn consumer_ref(&self) -> &AtomicU32 {
        unsafe { self.consumer.as_ref() }
    }

    fn ring_region_ref(&self) -> &[FrameDescriptor::RingType; RING_SIZE] {
        unsafe { self.ring_region_ptr.as_ref() }
    }

    fn ring_region_mut(&mut self) -> &mut [FrameDescriptor::RingType; RING_SIZE] {
        unsafe { self.ring_region_ptr.as_mut() }
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

    pub(crate) fn free_entries(&self) -> u32 {
        self.consumer()
            .wrapping_add(RING_SIZE as u32)
            .wrapping_sub(self.producer())
    }

    pub(crate) fn filled_entries(&self) -> u32 {
        self.producer().wrapping_sub(self.consumer())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.producer() == self.consumer()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.filled_entries() == RING_SIZE as u32
    }

    pub(crate) fn flags(&self) -> Option<XdpRingFlags> {
        let flags_ptr = self.flags?;
        // We need to read the value instead of creating a reference to avoid possibly violating Rust aliasing rules.
        // While we hold a non-mutable reference, the kernel might mutate the data.
        let flags = unsafe { flags_ptr.read() };
        Some(XdpRingFlags::from_bits_retain(flags))
    }

    pub(crate) fn needs_wakeup(&self) -> bool {
        if let Some(flags) = self.flags() {
            flags.contains(XdpRingFlags::XDP_RING_NEED_WAKEUP)
        } else {
            false
        }
    }

    pub(crate) fn pop(&mut self) -> Option<FrameDescriptor> {
        trace!("Popping.");
        if !self.is_empty() {
            let consumer = self.consumer();

            let desc = FrameDescriptor::from_ring_repr(
                self.ring_region_ref()[consumer as usize & (self.ring_region_ref().len() - 1)],
                self.umem_memory,
            );

            self.set_consumer(consumer.wrapping_add(1));

            Some(desc)
        } else {
            warn!("Popping failed, the ring is empty.");
            None
        }
    }

    pub(crate) fn push(&mut self, input: FrameDescriptor) -> Result<(), FrameDescriptor> {
        trace!("Pushing {:?}.", &input);

        if !self.is_full() {
            let producer = self.producer();

            let ring_region_len = self.ring_region_ref().len();

            self.ring_region_mut()[producer as usize & (ring_region_len - 1)] =
                input.into_ring_repr();

            self.set_producer(producer.wrapping_add(1));

            Ok(())
        } else {
            warn!("Pushing failed, ring full: {:?}", &input);
            Err(input)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::umem_packet_desc;
    use std::ptr::NonNull;
    use std::sync::atomic::AtomicU32;

    type RingType = u32;

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct MockFrameDescriptor(RingType);

    impl<Marker, const CHUNK_SIZE: usize> Descriptor<'_, Marker, CHUNK_SIZE> for MockFrameDescriptor where
        Marker: Debug
    {
    }
    impl<Marker, const CHUNK_SIZE: usize>
        umem_packet_desc::sealed::SealedDescriptorImpl<'_, Marker, CHUNK_SIZE>
        for MockFrameDescriptor
    where
        Marker: Debug,
    {
        type RingType = RingType;

        fn into_ring_repr(self) -> Self::RingType {
            0
        }

        fn from_desc(ring_repr: Self::RingType, _memory: NonNull<[u8; CHUNK_SIZE]>) -> Self {
            MockFrameDescriptor(ring_repr)
        }

        fn base_addr(_desc: &Self::RingType) -> u64 {
            0
        }
    }

    #[test]
    fn test_pop_empty_ring() {
        const RING_SIZE: usize = 4;
        const CHUNK_SIZE: usize = 1;

        let producer = AtomicU32::new(3);
        let consumer = AtomicU32::new(3);
        let mut ring_region = [RingType::default(); RING_SIZE];
        let umem_memory = NonNull::dangling();

        let mut inner_ring = InnerRing::<MockFrameDescriptor, (), CHUNK_SIZE, RING_SIZE>::new(
            NonNull::from(&producer),
            NonNull::from(&consumer),
            NonNull::from(&mut ring_region),
            None,
            umem_memory,
        );

        assert!(inner_ring.pop().is_none());
    }

    #[test]
    fn test_pop_single_element() {
        const RING_SIZE: usize = 4;
        const CHUNK_SIZE: usize = 1;

        let producer = AtomicU32::new(0);
        let consumer = AtomicU32::new(u32::MAX);
        let mut ring_region = [0_u32; RING_SIZE];
        ring_region[3] = 123;
        let umem_memory = NonNull::dangling();

        let mut inner_ring = InnerRing::<MockFrameDescriptor, (), CHUNK_SIZE, RING_SIZE>::new(
            NonNull::from(&producer),
            NonNull::from(&consumer),
            NonNull::from(&mut ring_region),
            None,
            umem_memory,
        );

        let popped = inner_ring.pop();
        assert_eq!(popped.unwrap(), MockFrameDescriptor(123));

        assert_eq!(inner_ring.consumer(), 0);
    }

    #[test]
    fn test_push() {
        const RING_SIZE: usize = 4;
        const CHUNK_SIZE: usize = 1;

        let producer = AtomicU32::new(0);
        let consumer = AtomicU32::new(0);
        let mut ring_region = [0_u32; RING_SIZE];
        let umem_memory = NonNull::dangling();

        let mut inner_ring = InnerRing::<MockFrameDescriptor, (), CHUNK_SIZE, RING_SIZE>::new(
            NonNull::from(&producer),
            NonNull::from(&consumer),
            NonNull::from(&mut ring_region),
            None,
            umem_memory,
        );

        let input = MockFrameDescriptor(1);
        assert!(inner_ring.push(input).is_ok());
        assert_eq!(inner_ring.producer(), 1);
        assert_eq!(ring_region[0], 0);

        assert!(inner_ring.push(input).is_ok());
        assert!(inner_ring.push(input).is_ok());
        assert!(inner_ring.push(input).is_ok());
        assert_eq!(inner_ring.producer(), 4);

        assert!(inner_ring.push(input).is_err());
        assert_eq!(inner_ring.producer(), 4);
    }
}
