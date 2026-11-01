use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor, SegmentSelector};
use lazy_static::lazy_static;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            // Kernel-owned stack region (src/stacks.rs): 2 MiB guard half
            // below the usable half; the paging commit leaves the guard PDE
            // non-present, so IST overflow faults instead of silently
            // corrupting neighbouring statics.
            VirtAddr::new(crate::stacks::double_fault_stack_top())
        };
        // Ring-3 transition stack (issue #19 stage 1): on every interrupt,
        // exception or `int 0x80` taken at CPL 3 the CPU switches to this
        // stack and pushes the interrupt frame on it (timer/keyboard IRQs
        // from user code land here too). Ring-0 execution never uses it.
        tss.privilege_stack_table[0] = VirtAddr::new(crate::stacks::syscall_stack_top());
        tss
    };
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.add_entry(Descriptor::kernel_code_segment());
        let data_selector = gdt.add_entry(Descriptor::kernel_data_segment());
        let tss_selector = gdt.add_entry(Descriptor::tss_segment(&TSS));
        // User segments (issue #19 stage 1): a flat ring-3 code/data pair
        // after the TSS. iretq to user mode loads CS/SS from these (with
        // RPL 3); they carry no base/limit of their own — isolation comes
        // from paging (U/S bits), not segmentation.
        let user_code_selector = gdt.add_entry(Descriptor::user_code_segment());
        let user_data_selector = gdt.add_entry(Descriptor::user_data_segment());
        (gdt, Selectors {
            code_selector,
            data_selector,
            tss_selector,
            user_code_selector,
            user_data_selector,
        })
    };
}

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
}

/// Ring-3 code segment selector (RPL 3), for the iretq frame into user mode.
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

/// Ring-3 data segment selector (RPL 3), for the SS field of the iretq
/// frame into user mode.
pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data_selector
}

pub fn init() {
    use x86_64::instructions::tables::load_tss;
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};

    GDT.0.load();
    unsafe {
        // Reload code segment
        CS::set_reg(GDT.1.code_selector);
        // Load TSS
        load_tss(GDT.1.tss_selector);
        // Set data segments to kernel data segment
        DS::set_reg(GDT.1.data_selector);
        ES::set_reg(GDT.1.data_selector);
        SS::set_reg(GDT.1.data_selector);
        // FS and GS can remain null for now
        FS::set_reg(SegmentSelector::new(0, x86_64::PrivilegeLevel::Ring0));
        GS::set_reg(SegmentSelector::new(0, x86_64::PrivilegeLevel::Ring0));
    }
}
