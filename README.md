# AF_XDP experiment

Exploration of an AF_XDP library.
The crate is working but unfinished and most certainly has bugs.

## Prerequisites

1. Install bpf-linker: `cargo install bpf-linker`

## Build eBPF

```bash
cargo xtask build-ebpf
```

## Run test

REQUIRES ROOT PRIVILEGES!

Requires the cargo runner to be set to `sudo -E`, see `xtask/src/run.rs`.
Currently only set for `x86_64-unknown-linux-gnu`, needs to be adapted for other platforms.

```bash
RUST_LOG=info cargo xtask run
```

## Random dev notes

- When sharing a UMEM between sockets, there can only be one socket for netdev-queue pairs other than the one the UMEM
  was registered with.
    - 1 UMEM registed to socket A.
    - Socket A has fill, comp, rx and tx ring bound to netdev 1 and queue 1.
    - Socket B shares the UMEM with socket A.
    - Socket B has fill, comp, rx and tx ring bound to netdev 1 and queue 2.
    - Binding socket C with rx and tx ring to netdev 1 and queue 2 sharing the same UMEM fails with EINVAL.
        - As far as I can see from the code, there doesn't seem to be a branch specifically for socket C in this setup.
          It's handled by the branch [1] that is executed for every socket sharing the UMEM but binding to a different
          netdev-queue pair than the orignal socket (A) used to register the UMEM. The `xp_assign_dev_shared` [2]
          function in that branch requires every socket to have a fill an comp ring, which leads to the EINVAL for
          socket B.
            - [1] https://github.com/torvalds/linux/blob/v6.12/net/xdp/xsk.c#L1221
            - [2] https://github.com/torvalds/linux/blob/v6.12/net/xdp/xsk_buff_pool.c#L253
- The socket fd can be added to the XSK map before the socket is bound. Is that intended?
