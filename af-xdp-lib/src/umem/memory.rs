use rustix::param::page_size;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;

#[derive(Debug)]
pub(crate) struct UmemMemory {
    memory: NonNull<u8>,
    number_of_chunks: usize,
    chunk_size: usize,
}
impl UmemMemory {
    pub(crate) fn new(number_of_chunks: usize, chunk_size: usize) -> Self {
        let layout = Layout::from_size_align(
            Self::allocation_length_internal(number_of_chunks, chunk_size),
            page_size(),
        )
        .unwrap();
        let umem_region = unsafe { alloc_zeroed(layout) };
        if umem_region.is_null() {
            handle_alloc_error(layout);
        }
        let memory = NonNull::new(umem_region).expect("umem region checked not to be null");

        Self {
            memory,
            number_of_chunks,
            chunk_size,
        }
    }

    pub(crate) fn allocation_length(&self) -> usize {
        Self::allocation_length_internal(self.chunk_size, self.number_of_chunks)
    }

    fn allocation_length_internal(number_of_chunks: usize, chunk_size: usize) -> usize {
        number_of_chunks * chunk_size
    }

    pub(crate) fn memory(&self) -> NonNull<u8> {
        self.memory
    }
}

impl Drop for UmemMemory {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.allocation_length(), page_size())
            .expect("Size and page size should not have changed since new()");
        unsafe { dealloc(self.memory.as_ptr(), layout) };
    }
}
