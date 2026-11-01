// Hardware device drivers. Each driver discovers its hardware either by
// probing the PCI bus (see pci.rs) or by fixed legacy ports.
//
// virtio-blk (issue #14) splits in two: `block` is the device-agnostic
// trait + access layer, `virtio_blk` the QEMU legacy PCI driver on top.

pub mod block;
pub mod pci;
pub mod virtio_blk;
