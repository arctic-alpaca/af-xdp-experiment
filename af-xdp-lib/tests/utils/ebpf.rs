use crate::utils::veth_netlink::VethPair;
use af_xdp_test_common::SOCKS_MAP_SIZE;
use aya::{Ebpf, include_bytes_aligned};
use aya_log::EbpfLogger;
use tracing::info;
use tracing_subscriber::EnvFilter;

pub async fn ebpf_test(
    test: fn(Ebpf, &mut VethPair),
    veth: impl Future<Output = VethPair>,
) -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    #[cfg(debug_assertions)]
    info!("Including debug BPF file.");
    #[cfg(debug_assertions)]
    let mut bpf = aya::EbpfLoader::new()
        // Sets the number of entries for the SOCKS map (XSKMAP).
        .map_max_entries("SOCKS", SOCKS_MAP_SIZE)
        .load(include_bytes_aligned!(
            "../../../target/bpfel-unknown-none/debug/af-xdp-test"
        ))?;

    #[cfg(not(debug_assertions))]
    info!("Including release BPF file.");
    #[cfg(not(debug_assertions))]
    let mut bpf = aya::BpfLoader::new()
        // Sets the number of entries for the SOCKS map (XSKMAP).
        .set_max_entries("SOCKS", SOCKS_MAP_SIZE)
        .load(include_bytes_aligned!(
            "../../../target/bpfel-unknown-none/release/af-xdp-test"
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

    let mut veth = veth.await;

    test(bpf, &mut veth);
    info!("Exiting...");
    Ok(())
}
