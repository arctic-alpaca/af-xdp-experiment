#![no_std]
#![no_main]

use af_xdp_test_common::SOCKS_MAP_SIZE;
use aya_ebpf::{
    bindings::xdp_action,
    macros::{map, xdp},
    maps::XskMap,
    programs::XdpContext,
};
use aya_log_ebpf::info;

#[map]
static SOCKS: XskMap = XskMap::with_max_entries(SOCKS_MAP_SIZE, 0);

#[xdp]
pub fn redirect_sock(ctx: XdpContext) -> u32 {
    let queue_id = unsafe { *ctx.ctx }.rx_queue_index;
    if SOCKS.get(queue_id) == Some(queue_id) {
        info!(&ctx, "Queue match on queue: {}", queue_id);
        match SOCKS.redirect(queue_id, 0) {
            Ok(ok_value) => {
                info!(&ctx, "ok_value: {}", ok_value);
                ok_value
            }
            Err(err_value) => {
                info!(&ctx, "err_value: {}", err_value);
                xdp_action::XDP_ABORTED
            }
        }
    } else {
        info!(
            &ctx,
            "No socket for queue {} at index {} in the XSKMAP", queue_id, queue_id
        );
        xdp_action::XDP_PASS
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
