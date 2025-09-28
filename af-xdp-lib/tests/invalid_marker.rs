mod utils;

use crate::utils::veth_netlink::{VethConfig, VethPair};
use af_xdp_lib::error::Error;
use af_xdp_lib::umem::Umem;
use aya::Ebpf;
use std::net::Ipv4Addr;

#[cfg(not(miri))]
#[tokio::test(flavor = "multi_thread")]
async fn invalid_marker() -> Result<(), anyhow::Error> {
    const NAME: &str = "marker";

    let veth = VethPair::new(
        format!("ns_{}", NAME),
        VethConfig::new(format!("n_{}", NAME), Ipv4Addr::new(10, 1, 0, 3), 1, 1),
        VethConfig::new(format!("o_{}", NAME), Ipv4Addr::new(10, 1, 0, 4), 1, 1),
    );
    utils::ebpf::ebpf_test(test, veth).await
}

pub fn test(_bpf: Ebpf, _veth: &mut VethPair) {
    struct Marker;
    let a = Umem::<Marker, 4096>::new(0, 1024).unwrap();
    assert_eq!(
        Umem::<Marker, 4096>::new(0, 1024).unwrap_err(),
        Error::MarkerAlreadyUsed
    );
    drop(a);
}
