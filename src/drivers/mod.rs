// Hardware device drivers. Each driver discovers its hardware either by
// probing the PCI bus (see pci.rs) or by fixed legacy ports.

pub mod pci;
