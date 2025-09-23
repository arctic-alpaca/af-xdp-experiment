mod utils;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::thread;
use std::time::Duration;

use crate::utils::veth_netlink;
use crate::utils::veth_netlink::VethPair;
use af_xdp_lib::umem::{DeviceId, QueueId, Rings, Umem};
use af_xdp_lib::umem_packet_desc::RxTxFrameDescriptor;
use af_xdp_test_common::SOCKS_MAP_SIZE;
use anyhow::Context;
use aya::Ebpf;
use aya::include_bytes_aligned;
use aya::maps::XskMap;
use aya::programs::{Xdp, XdpFlags};
use aya_log::EbpfLogger;
use mutnet::multi_step_parser::MultiStepParserResult;
use rustix::net::{AddressFamily, SocketType};
use tracing::debug;
use tracing::info;
use tracing_subscriber::EnvFilter;

const CHUNK_SIZE: usize = 4096;
const CHUNK_NUM: usize = 1024;

const RING_SIZE: u32 = 64;

const HEADROOM: usize = 100;

const QUEUE_ID: QueueId = QueueId(0);

#[cfg(not(miri))]
#[tokio::test(flavor = "multi_thread")]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Bump the memlock rlimit. This is needed for older kernels that don't use the
    // new memcg based accounting, see https://lwn.net/Articles/837122/
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {}", ret);
    }

    #[cfg(debug_assertions)]

    info!("Including debug BPF file.");
    #[cfg(debug_assertions)]
    let mut bpf = aya::EbpfLoader::new()
        // Sets the amount of entries for the SOCKS map (XSKMAP).
        .set_max_entries("SOCKS", SOCKS_MAP_SIZE)
        .load(include_bytes_aligned!(
            "../../target/bpfel-unknown-none/debug/af-xdp-test"
        ))?;

    #[cfg(not(debug_assertions))]
    info!("Including release BPF file.");
    #[cfg(not(debug_assertions))]
    let mut bpf = aya::BpfLoader::new()
        // Sets the amount of entries for the SOCKS map (XSKMAP).
        .set_max_entries("SOCKS", SOCKS_MAP_SIZE)
        .load(include_bytes_aligned!(
            "../../target/bpfel-unknown-none/release/af-xdp-test"
        ))?;

    let logger = EbpfLogger::init(&mut bpf).unwrap();

    let mut logger =
        tokio::io::unix::AsyncFd::with_interest(logger, tokio::io::Interest::READABLE).unwrap();
    tokio::task::spawn(async move {
        loop {
            let mut guard = logger.readable_mut().await.unwrap();
            guard.get_inner_mut().flush();
            guard.clear_ready();
        }
    });

    let veth = VethPair::new(
        "ve_A".to_owned(),
        Ipv4Addr::new(10, 0, 0, 1),
        "ve_B".to_owned(),
        Ipv4Addr::new(10, 0, 0, 2),
        "test".to_owned(),
        1,
        1,
        1,
        1,
    )
    .await;

    marker_main(bpf, &veth);
    info!("Exiting...");
    Ok(())
}

pub fn marker_main(mut bpf: Ebpf, veth: &VethPair) {
    veth.send_from_ns(10000, 1777, b"hello AF_XDP".to_vec());

    let socks: XskMap<_> = bpf.take_map("SOCKS").unwrap().try_into().unwrap();
    let program: &mut Xdp = bpf
        .program_mut("redirect_sock")
        .unwrap()
        .try_into()
        .unwrap();

    program.load().unwrap();
    program.attach(&veth.outside_veth_name, XdpFlags::default())
        .context("failed to attach the XDP program with default flags - try changing XdpFlags::default() to XdpFlags::SKB_MODE").unwrap();

    #[derive(Debug)]
    pub struct Marker;
    let name_to_index_socket =
        rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None).unwrap();
    let if_index_id = rustix::net::netdevice::name_to_index(
        name_to_index_socket.as_fd(),
        &veth.outside_veth_name,
    )
    .unwrap();

    info!("Creating UMEM.");
    let umem = Umem::<Marker, _, CHUNK_SIZE>::new(HEADROOM as u32, CHUNK_NUM).unwrap();

    info!("Getting descriptors.");
    let mut descriptors = umem.descriptors().unwrap();

    umem.add_net_dev(DeviceId(if_index_id), socks);
    info!("Getting rings.");
    let Rings {
        mut fill_ring,
        mut rx_ring,
        ..
    } = umem
        .rings::<{ RING_SIZE as usize }>(DeviceId(if_index_id), QUEUE_ID, QUEUE_ID.0)
        .unwrap();

    info!("Fill ring need wakeup: {}", fill_ring.needs_wakeup());

    for _ in 0..10 {
        let desc1 = descriptors.pop().unwrap();
        fill_ring.push(desc1).unwrap();
    }

    for _ in 0..10 {
        veth.send_from_ns(10000, 1777, b"hello AF_XDP".to_vec());
        if let Some(mut xdp_desc) = rx_ring.pop() {
            print_payload(&mut xdp_desc);
            fill_ring
                .push(xdp_desc.into_fill_comp_descriptor())
                .unwrap();
        }
        info!("RX ring pop failed expected");
        info!("RX ring need wakeup: {}", rx_ring.needs_wakeup());
        thread::sleep(Duration::from_millis(100));
    }

    program.unload().unwrap();
}

const BIND_PORT: u16 = 10000;
pub const RECIPIENT_PORT: u16 = 1777;
pub const BIND_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), BIND_PORT);
pub const RECIPIENT_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), RECIPIENT_PORT);

fn print_payload<const SIZE: usize, M: std::fmt::Debug>(
    xdp_desc: &mut RxTxFrameDescriptor<'_, M, SIZE>,
) {
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
