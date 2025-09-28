mod maker_guard;
pub(crate) mod memory;

use crate::descriptor::FillCompFrameDescriptor;
use crate::descriptor::sealed::SealedDescriptorImpl;
use crate::error::Error;
use crate::umem::maker_guard::MarkerGuard;
use crate::umem::memory::UmemMemory;
use rustix::net::sockopt::set_xdp_umem_reg;
use rustix::net::xdp::{
    SocketAddrXdp, SocketAddrXdpFlags, SocketAddrXdpWithSharedUmem, XdpUmemReg, XdpUmemRegFlags,
};
use rustix::net::{AddressFamily, SocketFlags, SocketType, bind, socket_with};
use std::any::type_name;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u32);

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub struct QueueId(pub u32);

/// Headroom reserved for the XDP frame by the driver.
///
/// See [this kernel mailing list post][mailing_list_post] for more info.
///
/// [mailing_list_post]: https://lore.kernel.org/xdp-newbies/CALDO+Sb00zQKuGKP43q-WEVXntMhmL+y8RN-_NTB879HxYbfTA@mail.gmail.com/
pub const XDP_FRAME_DRIVER_HEADROOM: usize = 256;

pub struct DescriptorsToken<Marker>(PhantomData<Marker>);

impl<Marker> Debug for DescriptorsToken<Marker> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("DescriptorsToken<{}>", type_name::<Marker>()))
            .finish()
    }
}

pub struct Umem<Marker, const CHUNK_SIZE: usize>
where
    Marker: 'static,
{
    memory: UmemMemory,
    initial_rings_given_out: AtomicBool,
    number_of_chunks: usize,
    socket: Arc<OwnedFd>,
    _marker_guard: MarkerGuard<Marker>,
}

impl<Marker, const CHUNK_SIZE: usize> Debug for Umem<Marker, CHUNK_SIZE> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("Umem<{}>", type_name::<Marker>()))
            .field("memory", &self.memory)
            .field("initial_rings_given_out", &self.initial_rings_given_out)
            .field("number_of_chunks", &self.number_of_chunks)
            .finish()
    }
}

unsafe impl<Marker, const CHUNK_SIZE: usize> Send for Umem<Marker, CHUNK_SIZE> {}
unsafe impl<Marker, const CHUNK_SIZE: usize> Sync for Umem<Marker, CHUNK_SIZE> {}

impl<Marker, const CHUNK_SIZE: usize> Umem<Marker, CHUNK_SIZE> {
    pub fn new(
        headroom: u32,
        number_of_chunks: usize,
    ) -> Result<(Self, DescriptorsToken<Marker>), Error> {
        let marker_guard = MarkerGuard::new()?;
        info!("Allocate memory.");

        let memory = UmemMemory::new(number_of_chunks, CHUNK_SIZE);

        let socket = Arc::new(socket_with(
            AddressFamily::XDP,
            SocketType::RAW,
            SocketFlags::CLOEXEC,
            None,
        )?);

        let umem = Self {
            memory,
            initial_rings_given_out: AtomicBool::new(false),
            socket,
            number_of_chunks,
            _marker_guard: marker_guard,
        };

        info!("Registering UMEM.");

        let umem_reg = XdpUmemReg {
            addr: umem.memory.memory().as_ptr() as u64,
            len: umem.memory.allocation_length() as u64,
            chunk_size: CHUNK_SIZE as u32,
            headroom,
            flags: XdpUmemRegFlags::empty(),
            tx_metadata_len: 0,
        };

        set_xdp_umem_reg(umem.socket.as_fd(), umem_reg)?;

        Ok((umem, DescriptorsToken(PhantomData)))
    }

    pub fn descriptors(
        &'_ self,
        token: DescriptorsToken<Marker>,
    ) -> Vec<FillCompFrameDescriptor<'_, Marker, CHUNK_SIZE>> {
        // Avoids having to prepend `_` to the variable, which would communicate it's not being used.
        let _ = token;

        (0..self.number_of_chunks)
            .map(|chunk_index| {
                let addr = (chunk_index * CHUNK_SIZE) as u64;
                FillCompFrameDescriptor::from_ring_repr(addr, &self.memory)
            })
            .rev()
            .collect()
    }

    pub(crate) fn bind_socket(
        &self,
        socket: Arc<OwnedFd>,
        net_device_id: DeviceId,
        queue_id: QueueId,
    ) -> rustix::io::Result<()> {
        if !self.initial_rings_given_out.swap(true, Ordering::AcqRel) {
            // The initial socket.
            let sockaddr_xdp = SocketAddrXdp::new(
                // TODO: Make this configurable.
                SocketAddrXdpFlags::XDP_USE_NEED_WAKEUP,
                net_device_id.0,
                queue_id.0,
            );
            bind(socket.as_fd(), &sockaddr_xdp)
        } else {
            // Follow-up socket.
            let sockaddr_xdp = SocketAddrXdpWithSharedUmem {
                addr: SocketAddrXdp::new(
                    SocketAddrXdpFlags::XDP_SHARED_UMEM,
                    net_device_id.0,
                    queue_id.0,
                ),
                shared_umem_fd: self.socket.as_fd(),
            };
            bind(socket.as_fd(), &sockaddr_xdp)
        }
    }

    pub(crate) fn xsk_map_socket(&self) -> Arc<OwnedFd> {
        if self.initial_rings_given_out.load(Ordering::Acquire) {
            let socket = socket_with(
                AddressFamily::XDP,
                SocketType::RAW,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            Arc::new(socket)
        } else {
            self.socket.clone()
        }
    }

    pub(crate) fn memory(&self) -> &UmemMemory {
        &self.memory
    }
}
