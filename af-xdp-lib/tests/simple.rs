mod utils;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::thread;
use std::time::Duration;

use crate::utils::veth_netlink::{VethConfig, VethPair};
use af_xdp_lib::descriptor::RxTxFrameDescriptor;
use af_xdp_lib::umem::{DeviceId, QueueId, Umem};
use af_xdp_lib::xsk_map;
use af_xdp_lib::xsk_map::XskMapStorage;
use anyhow::Context;
use aya::Ebpf;
use aya::maps::XskMap;
use aya::programs::{Xdp, XdpFlags};
use mutnet::multi_step_parser::MultiStepParserResult;
use rustix::net::{AddressFamily, SocketType, netdevice, socket};
use tracing::info;

const CHUNK_SIZE: usize = 4096;
const CHUNK_NUM: usize = 1024;

const RING_SIZE: usize = 64;

const HEADROOM: usize = 100;

const QUEUE_ID: QueueId = QueueId(0);

#[cfg(not(miri))]
#[tokio::test(flavor = "multi_thread")]
async fn simple() -> Result<(), anyhow::Error> {
    let veth = VethPair::new(
        "test".to_owned(),
        VethConfig::new("ve_A".to_owned(), Ipv4Addr::new(10, 0, 0, 1), 1, 1),
        VethConfig::new("ve_B".to_owned(), Ipv4Addr::new(10, 0, 0, 2), 1, 1),
    );
    utils::ebpf::ebpf_test(simple_test, veth).await
}

pub fn simple_test(mut bpf: Ebpf, veth: &mut VethPair) {
    veth.send_from_ns(BIND_PORT, RECIPIENT_PORT, b"hello AF_XDP".to_vec());

    let socks: XskMap<_> = bpf.take_map("SOCKS").unwrap().try_into().unwrap();
    let program: &mut Xdp = bpf
        .program_mut("redirect_sock")
        .unwrap()
        .try_into()
        .unwrap();

    program.load().unwrap();
    program.attach(&veth.outside_veth_name, XdpFlags::default())
        .context("failed to attach the XDP program with default flags - try changing XdpFlags::default() to XdpFlags::SKB_MODE").unwrap();

    pub struct Marker;
    let name_to_index_socket = socket(AddressFamily::INET, SocketType::DGRAM, None).unwrap();
    let if_index_id =
        netdevice::name_to_index(name_to_index_socket.as_fd(), &veth.outside_veth_name).unwrap();

    info!("Creating UMEM.");
    let (umem, descriptors_token) =
        Umem::<Marker, CHUNK_SIZE>::new(HEADROOM as u32, CHUNK_NUM).unwrap();

    let xsk_map = XskMapStorage::new(socks, DeviceId(if_index_id), &umem);

    info!("Getting descriptors.");
    let mut descriptors = umem.descriptors(descriptors_token);

    info!("Getting rings.");
    let xsk_map::Rings::Four(mut rings) = xsk_map.rings::<RING_SIZE>(QUEUE_ID, QUEUE_ID.0) else {
        panic!("Failed to get rings");
    };

    let xsk_map::Rings::Two(mut _rings2) = xsk_map.rings::<RING_SIZE>(QUEUE_ID, 1) else {
        panic!("Failed to get rings");
    };

    let (rings2_rx, rings2_tx) = _rings2.rings();

    thread::scope(|scope| {
        scope.spawn(|| {
            println!("scoped thread: {}", rings2_rx.needs_wakeup());
        });
        scope.spawn(|| {
            println!("scoped thread: {}", rings2_tx.needs_wakeup());
        });

        scope.spawn(|| {
            info!(
                "Fill ring need wakeup: {}",
                rings.fill_ring().needs_wakeup()
            );

            for _ in 0..10 {
                let desc1 = descriptors.pop().unwrap();
                rings.fill_ring().push(desc1).unwrap();
            }

            for _ in 0..10 {
                veth.send_from_ns(BIND_PORT, RECIPIENT_PORT, b"hello AF_XDP".to_vec());
                if let Some(mut xdp_desc) = rings.rx_ring().pop() {
                    print_payload(&mut xdp_desc);
                    rings.fill_ring().push(xdp_desc.into()).unwrap();
                }
                info!("RX ring pop failed as expected");
                info!("RX ring needs wakeup: {}", rings.rx_ring().needs_wakeup());
                thread::sleep(Duration::from_millis(100));
            }
        });
    });

    program.unload().unwrap();
}

const BIND_PORT: u16 = 10000;
pub const RECIPIENT_PORT: u16 = 1777;
pub const BIND_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), BIND_PORT);
pub const RECIPIENT_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), RECIPIENT_PORT);

fn print_payload<const SIZE: usize, Marker>(xdp_desc: &mut RxTxFrameDescriptor<'_, Marker, SIZE>) {
    use mutnet::arp::ArpMethods;
    let data_start = xdp_desc.data_offset();
    let buf = &mut xdp_desc.memory_mut()[data_start - HEADROOM..data_start + HEADROOM + 150];

    let packet_data =
        mutnet::multi_step_parser::parse_network_data::<_, 10>(buf, HEADROOM, false, false, false)
            .unwrap();
    println!("{:?}", packet_data);
    if let MultiStepParserResult::ArpEth(data_buffer) = packet_data {
        info!("protocol type: {:?}", data_buffer.arp_typed_protocol_type());
        info!(
            "Operation code: {:?}",
            data_buffer.arp_typed_operation_code()
        );
        info!(
            "sender protocol address: {:?}",
            data_buffer.arp_sender_protocol_address()
        );
        info!(
            "target protocol address: {:?}",
            data_buffer.arp_target_protocol_address()
        );
    } else {
        info!("{:?}", xdp_desc);
    }
}
