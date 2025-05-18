use futures::TryStreamExt;
use rtnetlink::packet_route::link::LinkAttribute;
use rtnetlink::packet_route::neighbour::NeighbourState;
use rtnetlink::{Handle, LinkUnspec, LinkVeth, NetworkNamespace, RouteMessageBuilder};
use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};

use ethtool::EthtoolHandle;
use std::thread;

use tracing::info;

pub const NETNS_PATH: &str = "/run/netns/";

pub struct VethPair {
    pub outside_veth_name: String,
    netns_name: String,
    netns_file_path: PathBuf,
    outside_veth_ip: Ipv4Addr,
    namespaced_veth_ip: Ipv4Addr,
}

#[allow(clippy::too_many_arguments)]
impl VethPair {
    pub async fn new(
        outside_veth_name: String,
        outside_veth_ip: Ipv4Addr,
        namespaced_veth_name: String,
        namespaced_veth_ip: Ipv4Addr,
        netns_name: String,
        outside_veth_rx_count: u32,
        outside_veth_tx_count: u32,
        namespaced_veth_rx_count: u32,
        namespaced_veth_tx_count: u32,
    ) -> Self {
        let netns_file_path = Self::create_netns(&netns_name).await;

        let veth_pair = Self {
            outside_veth_name: outside_veth_name.clone(),
            netns_name,
            netns_file_path: netns_file_path.clone(),
            outside_veth_ip,
            namespaced_veth_ip,
        };

        let (connection, handle, _) = rtnetlink::new_connection().unwrap();
        tokio::spawn(connection);
        let netns_handle =
            Self::open_connection_in_netns(netns_file_path.clone(), rtnetlink::new_connection)
                .await;

        Self::create_veths(&outside_veth_name, &namespaced_veth_name, &handle).await;

        let (outside_veth_index, outside_veth_mac) =
            Self::get_if_index(&outside_veth_name, &handle)
                .await
                .unwrap();
        let (namespaced_veth_index, _namespaced_veth_mac) =
            Self::get_if_index(&namespaced_veth_name, &handle)
                .await
                .unwrap();

        Self::move_veth_into_netns(&namespaced_veth_name, &netns_file_path, &handle).await;
        Self::add_address(outside_veth_index, outside_veth_ip, &handle).await;

        Self::add_address(namespaced_veth_index, namespaced_veth_ip, &netns_handle).await;

        Self::set_up(outside_veth_index, &handle).await;
        Self::set_up(namespaced_veth_index, &netns_handle).await;

        Self::add_neighbour(
            namespaced_veth_index,
            &outside_veth_mac,
            outside_veth_ip,
            &netns_handle,
        )
        .await;

        Self::add_default_route(outside_veth_ip, &netns_handle).await;

        let (connection, mut eth_handle, _) = ethtool::new_connection().unwrap();
        tokio::spawn(connection);

        let mut netns_eth_handle =
            Self::open_connection_in_netns(netns_file_path, ethtool::new_connection).await;

        Self::set_rx_tx_channels(
            &outside_veth_name,
            outside_veth_rx_count,
            outside_veth_tx_count,
            &mut eth_handle,
        )
        .await;

        Self::set_rx_tx_channels(
            &namespaced_veth_name,
            namespaced_veth_rx_count,
            namespaced_veth_tx_count,
            &mut netns_eth_handle,
        )
        .await;

        veth_pair
    }

    async fn create_netns(netns_name: &str) -> PathBuf {
        NetworkNamespace::add(netns_name.to_owned()).await.unwrap();
        [NETNS_PATH, netns_name].iter().collect()
    }

    async fn create_veths(veth_1_name: &str, veth_2_name: &str, handle: &Handle) {
        handle
            .link()
            .add(LinkVeth::new(veth_1_name, veth_2_name).build())
            .execute()
            .await
            .unwrap();
    }

    async fn move_veth_into_netns(veth_name: &str, netns_file_path: &Path, handle: &Handle) {
        let netns_file = File::open(netns_file_path).unwrap();
        handle
            .link()
            .set(
                LinkUnspec::new_with_name(veth_name)
                    .setns_by_fd(netns_file.as_raw_fd())
                    .build(),
            )
            .execute()
            .await
            .unwrap();
    }

    async fn add_address(veth_index: u32, ip_addr: Ipv4Addr, handle: &Handle) {
        handle
            .address()
            .add(veth_index, ip_addr.into(), 24)
            .execute()
            .await
            .unwrap();
    }

    async fn set_up(veth_index: u32, handle: &Handle) {
        handle
            .link()
            .set(LinkUnspec::new_with_index(veth_index).up().build())
            .execute()
            .await
            .unwrap();
    }

    async fn add_neighbour(veth_index: u32, veth_mac: &[u8], veth_ip: Ipv4Addr, handle: &Handle) {
        handle
            .neighbours()
            .add(veth_index, veth_ip.into())
            .link_local_address(veth_mac)
            .state(NeighbourState::Permanent)
            .execute()
            .await
            .unwrap();
    }

    async fn add_default_route(veth_ip: Ipv4Addr, handle: &Handle) {
        handle
            .route()
            .add(
                RouteMessageBuilder::<Ipv4Addr>::new()
                    .gateway(veth_ip)
                    .build(),
            )
            .execute()
            .await
            .unwrap();
    }

    async fn set_rx_tx_channels(
        veth_name: &str,
        rx_count: u32,
        tx_count: u32,
        handle: &mut EthtoolHandle,
    ) {
        handle
            .channel()
            .set(veth_name)
            .rx_count(rx_count)
            .tx_count(tx_count)
            .execute()
            .await
            .unwrap();
    }
    async fn get_if_index(
        if_name: &str,
        handle: &Handle,
    ) -> Result<(u32, Vec<u8>), rtnetlink::Error> {
        let info = handle
            .link()
            .get()
            .match_name(if_name.to_owned())
            .execute()
            .try_next()
            .await
            .unwrap()
            .unwrap();
        let index = info.header.index;
        let mac = info
            .attributes
            .iter()
            .find_map(|attr| {
                if let LinkAttribute::Address(mac) = attr {
                    Some(mac.clone())
                } else {
                    None
                }
            })
            .unwrap();
        Ok((index, mac))
    }

    async fn open_connection_in_netns<T, U, V: 'static, W>(
        netns_file_path: PathBuf,
        f: fn() -> Result<(T, U, V), W>,
    ) -> U
    where
        T: Send + Future + 'static,
        U: Send + 'static,
        <T as Future>::Output: Send,
        W: std::fmt::Debug + 'static,
    {
        let tokio_runtime_handle = tokio::runtime::Handle::current();
        thread::spawn(move || {
            let netns_file = File::open(netns_file_path).unwrap();

            // Move the thread into the network namespace.
            rustix::thread::move_into_link_name_space(netns_file.as_fd(), None).unwrap();

            // The connection socket must be created in the context of the Tokio runtime.
            let _guard = tokio_runtime_handle.enter();
            let (connection, handle, _) = tokio_runtime_handle.block_on(async { f().unwrap() });
            tokio::spawn(connection);
            handle
        })
        .join()
        .unwrap()
    }

    pub(crate) fn send_from_ns(&self, send_from_port: u16, send_to_port: u16, payload: Vec<u8>) {
        let netns_file_path = self.netns_file_path.clone();
        let outside_veth_ip = self.outside_veth_ip;
        let namespaced_veth_ip = self.namespaced_veth_ip;
        thread::spawn(move || {
            let file = File::open(netns_file_path).unwrap();
            rustix::thread::move_into_link_name_space(file.as_fd(), None).unwrap();

            let sock = UdpSocket::bind(SocketAddr::new(namespaced_veth_ip.into(), send_from_port))
                .unwrap();
            sock.send_to(
                payload.as_slice(),
                SocketAddr::new(outside_veth_ip.into(), send_to_port),
            )
            .unwrap();

            info!("Sent data from thread");
        });
    }
}

impl Drop for VethPair {
    fn drop(&mut self) {
        let netns_name = self.netns_name.clone();
        let tokio_runtime_handle = tokio::runtime::Handle::current();

        thread::spawn(move || {
            tokio_runtime_handle
                .block_on(async move { NetworkNamespace::del(netns_name).await.unwrap() });
        })
        .join()
        .unwrap();
    }
}
