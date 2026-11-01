//! Cooperative kernel tasks and a round-robin scheduler (issue #18, stage 1).
//!
//! A [`Task`] is a named unit of kernel work with its own 64 KiB heap-allocated
//! kernel stack. Scheduling is strictly cooperative: a running task keeps the
//! CPU until it calls [`yield_now`] (or returns from its entry, which exits it),
//! at which point the scheduler switches to the next `Ready` task round-robin.
//! The kernel main loop (`kernel::kernel_main_owned`) plays the role of the
//! "main" task (slot 0): it drains keyboard/serial input as before and runs the
//! scheduler whenever other tasks are ready, falling back to `enable_and_hlt`
//! when only it remains.
//!
//! # Context switch
//!
//! **The `x86_64-unknown-uefi` target uses the Microsoft x64 ABI, not SysV.**
//! A switch at a *function call boundary* must therefore preserve exactly the
//! MS-x64 non-volatile state: `rbx`, `rbp`, `rdi`, `rsi`, `r12`–`r15`, and
//! `xmm6`–`xmm15` (integer args arrive in `rcx`/`rdx`/`r8`/`r9`; everything
//! else — rax, rcx, rdx, r8-r11, xmm0-5 — is volatile and dead at a yield
//! point). The kernel does not touch x87/MXCSR state, and RFLAGS.IF is
//! preserved in software ([`schedule`] disables interrupts around the switch
//! and restores its own IF on resume; DF stays clear as Rust keeps it), so no
//! flags spill is needed. The switch itself is hand-written assembly (see
//! `task_switch` below).
//!
//! # Stack layout and alignment
//!
//! A fresh task never "returns into" a caller: its stack is initialized so the
//! final `ret` of the switch lands in [`task_trampoline`], an `extern "C"`
//! function that invokes the task entry and then calls [`exit_current`]. With
//! `T` the initial top of the task's stack — the highest 16-byte-aligned
//! address inside the allocation, i.e. `stack_end - 16` — the initial image is:
//!
//! ```text
//! T + 0     trampoline address  <- "return address" popped by ret
//! T - 8     rbx = 0   \
//! T - 16    rbp = 0   |
//! T - 24    rdi = 0   |  integer non-volatiles, popped by the switch-in
//! T - 32    rsi = 0   |  path in the same relative order as the
//! T - 40    r12 = 0   |  switch-out pushes
//! T - 48    r13 = 0   |
//! T - 56    r14 = 0   |
//! T - 64    r15 = 0   /
//! T - 224   xmm6..xmm15 area, 10 x 16 bytes, zero-filled
//!           (context.rsp points here; the switch-in add restores it)
//! ```
//!
//! `T` cannot be `stack_end` itself: the return-address slot must be a
//! WRITABLE byte (one past the allocation is not), and it must be 16-aligned.
//! Hence the reserved 16 bytes at the top of every task stack.
//!
//! The MS x64 ABI requires the stack to be 16-byte aligned immediately before
//! a `call`, i.e. RSP ≡ 8 (mod 16) at function entry. `T` is 16-byte aligned,
//! so after the switch's `ret` pops the trampoline address, RSP = `T + 8` —
//! exactly the post-`call` entry state, and the trampoline behaves like an
//! ordinarily-called function. For a *resumed* task the same `ret` pops the
//! return address its own `task_switch` call pushed, which restores the
//! standard alignment by construction.
//!
//! # Locking and context rules
//!
//! - The scheduler lock is thread-context only. It is NEVER taken from an
//!   ISR, and the switch runs with interrupts disabled so no IRQ can observe
//!   a half-switched RSP (issue #2 rules: no console/FS/allocator work from
//!   ISRs either, so an ISR can never reach this module indirectly).
//! - Task entries run in thread context and may use the terminal, serial, FS,
//!   and the heap. They must NOT call into the shell or take the SHELL lock,
//!   and they must never disable interrupts around a yield.
//! - **Interrupt state of a task**: a RESUMED task restores the IF its own
//!   `schedule()` call saved; a FRESH task never runs that epilogue, so it
//!   inherits the IF of the context that first scheduled it (`start_if`,
//!   applied by the trampoline) — enabled when spawned from the shell/main
//!   loop, disabled when driven by the test harness. A task entry may
//!   therefore be interrupted: IRQ handlers (timer/keyboard/serial) run and
//!   return normally, but nothing is ever scheduled from an ISR (stage 2
//!   would change that).
//! - **Never yield while holding a lock another task can take**: a yield
//!   with the terminal, serial-TX, FS, heap, or scheduler lock held can
//!   deadlock the single CPU when the next task blocks on the same lock.
//! - A task that panics is fatal (panic = abort; there is no task teardown).
//!
//! # Remaining gap (stage 2)
//!
//! Timer-driven preemption (issue #18 stage 2, needs #7 PIT configuration on
//! top of the current tick-only ISR): preemption must additionally save the
//! full interrupt-frame register state (caller-saved registers and RFLAGS),
//! which the x86-interrupt ABI already spills onto the interrupted stack —
//! the switch would move the whole frame to the preempted task's stack. No
//! userland is implemented or planned for this module.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;

/// Size of one task kernel stack: 64 KiB.
const STACK_SIZE: usize = 64 * 1024;
/// Alignment of a task stack allocation (and therefore of its top).
const STACK_ALIGN: usize = 16;

const STACK_LAYOUT: Layout = match Layout::from_size_align(STACK_SIZE, STACK_ALIGN) {
    Ok(layout) => layout,
    Err(_) => panic!("invalid task stack layout"),
};

// ---------------------------------------------------------------------------
// Context switch
// ---------------------------------------------------------------------------

/// Saved non-volatile register state of a switched-out task.
///
/// `rsp` (offset 0) is the only field the switch accesses by name: it parks
/// the task's RSP *below* the 160-byte `xmm6`–`xmm15` spill area, so the
/// switch-in side restores the XMM registers first, then walks up through the
/// integer registers to the return address. The remaining fields document the
/// integer save area and are kept for debuggability; they are zero for a
/// fresh task.
#[allow(dead_code)] // register fields are touched only by the task_switch asm
#[repr(C)]
struct TaskContext {
    rsp: u64,
    rbx: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

/// Bytes of XMM spill area (xmm6..xmm15, 16 bytes each). Part of every saved
/// context: Microsoft x64 declares xmm6-xmm15 non-volatile.
const XMM_AREA: usize = 10 * 16;

global_asm!(
    "
    .text
    .globl task_switch
task_switch:
    // Microsoft x64 ABI: rcx = &old.context, rdx = &new.context.
    // Save the outgoing task's non-volatile state. Integer pushes mirror the
    // switch-in pops below (rbx highest .. r15 lowest); the XMM area is
    // carved out with sub/movdqu (unaligned is fine — the heap stack is
    // ordinary RAM and movdqu avoids any alignment arithmetic).
    push rbx
    push rbp
    push rdi
    push rsi
    push r12
    push r13
    push r14
    push r15
    sub rsp, 160
    movdqu [rsp + 0], xmm6
    movdqu [rsp + 16], xmm7
    movdqu [rsp + 32], xmm8
    movdqu [rsp + 48], xmm9
    movdqu [rsp + 64], xmm10
    movdqu [rsp + 80], xmm11
    movdqu [rsp + 96], xmm12
    movdqu [rsp + 112], xmm13
    movdqu [rsp + 128], xmm14
    movdqu [rsp + 144], xmm15
    // Park the outgoing RSP at offset 0 of *old (TaskContext.rsp).
    mov [rcx], rsp
    // Adopt the incoming task's RSP. For a fresh task this points at its
    // zeroed XMM area (see the module docs); for a resumed task, at the area
    // this function carved out when switching that task out.
    mov rsp, [rdx]
    movdqu xmm15, [rsp + 144]
    movdqu xmm14, [rsp + 128]
    movdqu xmm13, [rsp + 112]
    movdqu xmm12, [rsp + 96]
    movdqu xmm11, [rsp + 80]
    movdqu xmm10, [rsp + 64]
    movdqu xmm9, [rsp + 48]
    movdqu xmm8, [rsp + 32]
    movdqu xmm7, [rsp + 16]
    movdqu xmm6, [rsp + 0]
    add rsp, 160
    pop r15
    pop r14
    pop r13
    pop r12
    pop rsi
    pop rdi
    pop rbp
    pop rbx
    // Resume the incoming task: pops its saved return address. For a fresh
    // task that is the trampoline; for a resumed task, the instruction
    // after its own task_switch call.
    ret
",
);

extern "C" {
    fn task_switch(old: *mut TaskContext, new: *const TaskContext);
}

/// Entry shim every fresh task starts in. Runs with the fresh task's brand-new
/// stack (see the module docs for the exact entry state) and with the
/// interrupt state the scheduling context recorded for the first run — a
/// fresh task never executes `schedule()`'s epilogue, so it must restore its
/// own IF here before the entry runs. Calls the task's entry, then exits the
/// task — the stack below the trampoline frame is dead from here on and is
/// reclaimed by the scheduler once some other task has switched away from it.
#[no_mangle]
extern "C" fn task_trampoline() -> ! {
    let (entry, start_if) = {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        let task = &mut scheduler.tasks[current];
        task.started = true;
        (task.entry, task.start_if)
    };
    if start_if {
        x86_64::instructions::interrupts::enable();
    }
    entry();
    exit_current();
}

// ---------------------------------------------------------------------------
// Task and scheduler state
// ---------------------------------------------------------------------------

/// Task identifier. 0 is the main task (the kernel main loop).
pub type TaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Runnable, waiting for the scheduler to pick it.
    Ready,
    /// Currently executing (the single CPU's owner).
    Running,
    /// Entry returned (or [`exit_current`] was called); stack already
    /// reclaimable. Kept in the table so `tasks` can report history.
    Exited,
}

/// A heap stack owned by one task. Deallocated on drop, which the scheduler
/// triggers (reaping) only once the task is no longer running anywhere.
struct TaskStack {
    ptr: *mut u8,
}

impl TaskStack {
    /// Allocate a zero-filled, 16-byte-aligned task stack from the heap.
    fn alloc() -> Option<TaskStack> {
        let ptr = unsafe { alloc_zeroed(STACK_LAYOUT) };
        if ptr.is_null() {
            None
        } else {
            Some(TaskStack { ptr })
        }
    }

    /// Highest 16-byte-aligned address that can hold the initial context's
    /// return-address slot while staying INSIDE the allocation: `ptr +
    /// STACK_SIZE` itself is one past the last writable byte, and the slot
    /// must be 16-aligned so the post-`ret` RSP (= slot + 8) is ≡ 8 (mod 16)
    /// as the ABI requires at function entry.
    fn initial_top(&self) -> *mut u64 {
        unsafe { self.ptr.add(STACK_SIZE - 16) as *mut u64 }
    }
}

impl Drop for TaskStack {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, STACK_LAYOUT) };
    }
}

// Safety: `ptr` is an owning handle to a `STACK_LAYOUT` allocation created in
// [`TaskStack::alloc`] and freed exactly once in `drop`. Moving a `TaskStack`
// (e.g. when the scheduler's `Vec` grows) moves only the pointer, never the
// stack bytes, and the bytes are touched only by the owning task while it
// runs — scheduler state is thread-context only (see the module docs).
unsafe impl Send for TaskStack {}

struct Task {
    id: TaskId,
    name: String,
    state: TaskState,
    entry: extern "C" fn(),
    /// Whether the task has ever been switched to. A fresh (never-started)
    /// task takes its interrupt state from the context that first schedules
    /// it, because it never runs `schedule()`'s resume epilogue.
    started: bool,
    /// IF to restore for the first run; recorded by [`schedule`]/
    /// [`exit_current`] at selection time and consumed by the trampoline.
    start_if: bool,
    /// Owned 64 KiB stack; `None` once reaped (exited tasks only) or for the
    /// main task, which runs on the static kernel stack from `stacks.rs`.
    stack: Option<TaskStack>,
    context: TaskContext,
}

impl Task {
    /// Write the initial context of a fresh task onto the top of `stack`:
    /// the trampoline address as the "return address", zeroed integer
    /// non-volatiles below it, and a zeroed XMM spill area below that (the
    /// stack comes zero-filled from the heap allocator, but the slots are
    /// written explicitly so the layout is self-documenting). The exact
    /// layout, and why `context.rsp` ends up at `top - 224`, is in the
    /// module docs. Must run before the task is ever switched to.
    unsafe fn prime_context(&mut self) {
        let top = self
            .stack
            .as_ref()
            .expect("fresh task must own a stack")
            .initial_top();
        unsafe {
            top.write(task_trampoline as extern "C" fn() -> ! as usize as u64); // +0:    return address
            top.sub(1).write(0); // -8:    rbx
            top.sub(2).write(0); // -16:   rbp
            top.sub(3).write(0); // -24:   rdi
            top.sub(4).write(0); // -32:   rsi
            top.sub(5).write(0); // -40:   r12
            top.sub(6).write(0); // -48:   r13
            top.sub(7).write(0); // -56:   r14
            top.sub(8).write(0); // -64:   r15
            // -224..-64: XMM area, already zero-filled with the stack.
            self.context = TaskContext {
                rsp: top.byte_sub(XMM_AREA + 8 * 8) as u64, // -224 (xmm area + 8 int slots)
                rbx: 0,
                rbp: 0,
                rdi: 0,
                rsi: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
            };
        }
    }
}

struct Scheduler {
    /// All tasks ever spawned. Index 0 is the main task. Grows only from
    /// `spawn` (thread context); raw pointers into `tasks` are formed and
    /// consumed inside a single `schedule`/`exit_current` call with
    /// interrupts disabled, where no push can interleave.
    tasks: Vec<Task>,
    /// Index into `tasks` of the currently running task.
    current: usize,
    next_id: TaskId,
}

impl Scheduler {
    fn new() -> Scheduler {
        Scheduler {
            tasks: vec![Task {
                id: 0,
                name: String::from("main"),
                state: TaskState::Running,
                // Never called: the main task is switched back into, never
                // started through the trampoline (its "entry" is the main
                // loop that called into the scheduler).
                entry: main_task_entry,
                started: true,
                start_if: false, // never consulted; main resumes via epilogues
                stack: None,
                context: TaskContext {
                    rsp: 0,
                    rbx: 0,
                    rbp: 0,
                    rdi: 0,
                    rsi: 0,
                    r12: 0,
                    r13: 0,
                    r14: 0,
                    r15: 0,
                },
            }],
            current: 0,
            next_id: 1,
        }
    }

    /// Reclaim the stacks of exited tasks. The current task is Running here
    /// (only [`schedule`] reaps), so the live stack is never freed.
    fn reap_exited(&mut self) {
        for task in &mut self.tasks {
            if task.state == TaskState::Exited {
                task.stack = None; // Drop deallocates.
            }
        }
    }

    /// Round-robin pick of the next Ready task after `current`, marking the
    /// running task Ready and the pick Running. `None` when nothing else is
    /// ready (the main task is Ready whenever it is not running, so with only
    /// the main task in the table this returns `None` exactly when idle).
    fn pick_next(&mut self) -> Option<usize> {
        let count = self.tasks.len();
        let old = self.current;
        for step in 1..count {
            let candidate = (old + step) % count;
            if self.tasks[candidate].state == TaskState::Ready {
                if self.tasks[old].state == TaskState::Running {
                    self.tasks[old].state = TaskState::Ready;
                }
                self.tasks[candidate].state = TaskState::Running;
                self.current = candidate;
                return Some(candidate);
            }
        }
        None
    }
}

extern "C" fn main_task_entry() {
    // Unreachable by construction: slot 0 is never started via the
    // trampoline. Kept as a named function so `Task.entry` is always valid.
    panic!("main task cannot be started through the trampoline");
}

lazy_static! {
    /// Scheduler state. Thread-context only (see module docs): ISRs never
    /// touch it, so the lock serializes the main loop and task entries.
    static ref SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());
}

/// Set once by [`init`]; `spawn`/`schedule` refuse to run before it so boot
/// ordering (heap before scheduler) stays explicit.
static INITTED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Public API (thread context only)
// ---------------------------------------------------------------------------

/// Create the scheduler and its main task. Idempotent; must run after the
/// heap is up (task stacks are heap allocations) and before the first spawn.
pub fn init() {
    lazy_static::initialize(&SCHEDULER);
    INITTED.store(true, Ordering::Relaxed);
}

/// Spawn a named task running `entry` on a fresh 64 KiB stack. The task
/// becomes Ready; it first runs when some thread-context caller (the main
/// loop or another task's yield) next invokes [`schedule`].
pub fn spawn(name: &str, entry: extern "C" fn()) -> Result<TaskId, &'static str> {
    if !INITTED.load(Ordering::Relaxed) {
        return Err("task::init() has not run yet");
    }
    let stack = TaskStack::alloc().ok_or("out of memory for task stack")?;
    let mut scheduler = SCHEDULER.lock();
    let id = scheduler.next_id;
    scheduler.next_id += 1;
    let mut task = Task {
        id,
        name: String::from(name),
        state: TaskState::Ready,
        entry,
        started: false,
        start_if: false, // set at first selection, from the scheduling context
        stack: Some(stack),
        context: TaskContext {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            rdi: 0,
            rsi: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        },
    };
    // Safety: the stack is freshly allocated, exclusively owned by `task`,
    // and the task has never run, so nothing else can touch it yet.
    unsafe { task.prime_context() };
    scheduler.tasks.push(task);
    Ok(id)
}

/// Run the next Ready task, if any; returns once the current task is running
/// again (or immediately when nothing else is ready). Thread context only.
///
/// Interrupts are disabled across the decision and the switch so no ISR can
/// fire on a half-switched stack, and each task's interrupt state is
/// preserved across its own switches (see the module docs).
pub fn schedule() {
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let switch = {
        let mut scheduler = SCHEDULER.lock();
        scheduler.reap_exited();
        let old = scheduler.current;
        scheduler.pick_next().map(|next| {
            // A task that has never run does not resume through a schedule()
            // epilogue, so it inherits THIS scheduling context's interrupt
            // state; the trampoline applies it before the entry.
            if !scheduler.tasks[next].started {
                scheduler.tasks[next].start_if = interrupts_were_enabled;
            }
            // Raw pointers stay valid for this call: interrupts are disabled,
            // only thread context runs, and nothing can push to `tasks`
            // before task_switch completes.
            let old_context = &mut scheduler.tasks[old].context as *mut TaskContext;
            let new_context = &scheduler.tasks[scheduler.current].context
                as *const TaskContext;
            (old_context, new_context)
        })
    };

    if let Some((old_context, new_context)) = switch {
        // Safety: both pointers point into stable heap slots (no push can
        // interleave), and the register/stack state at both ends satisfies
        // the ABI contract documented on TaskContext and in the module docs.
        unsafe { task_switch(old_context, new_context) };
    }

    if interrupts_were_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// Yield the CPU: schedule another Ready task if one exists.
pub fn yield_now() {
    schedule();
}

/// Mark the current task Exited and switch away from it. Never returns to the
/// caller's context — the stack is reclaimed once another schedule() reaps it.
/// Only valid for spawned tasks; the main task (id 0) must never exit.
fn exit_current() -> ! {
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let switch = {
        let mut scheduler = SCHEDULER.lock();
        let old = scheduler.current;
        if scheduler.tasks[old].id == 0 {
            panic!("the main task cannot exit");
        }
        scheduler.tasks[old].state = TaskState::Exited;
        scheduler.pick_next().map(|next| {
            // Same first-run rule as schedule(): a never-started pick
            // inherits the exiting context's interrupt state.
            if !scheduler.tasks[next].started {
                scheduler.tasks[next].start_if = interrupts_were_enabled;
            }
            // The exited task's context is dead; only the new task's context
            // (and the old rsp slot, written and immediately abandoned) is
            // touched by the switch.
            let old_context = &mut scheduler.tasks[old].context as *mut TaskContext;
            let new_context = &scheduler.tasks[scheduler.current].context
                as *const TaskContext;
            (old_context, new_context)
        })
    };

    match switch {
        Some((old_context, new_context)) => {
            // Same safety argument as `schedule`. This `task_switch` never
            // returns into `exit_current`: the outgoing context is Exited and
            // is never picked again.
            unsafe { task_switch(old_context, new_context) };
        }
        None => panic!("exit_current: no task to switch to (main task missing?)"),
    }
    unreachable!("task_switch returned into an exited task");
}

/// Whether any task other than the currently running one is Ready. Cheap
/// main-loop probe: drives the "schedule instead of halt" decision.
pub fn has_ready_tasks() -> bool {
    let scheduler = SCHEDULER.lock();
    scheduler
        .tasks
        .iter()
        .enumerate()
        .any(|(index, task)| index != scheduler.current && task.state == TaskState::Ready)
}

/// Id of the currently running task (0 = main).
pub fn current_id() -> TaskId {
    let scheduler = SCHEDULER.lock();
    scheduler.tasks[scheduler.current].id
}

/// Snapshot of the task table plus the current task's id, for the `tasks`
/// shell command. Allocates (thread context).
pub fn snapshot() -> (Vec<(TaskId, String, TaskState)>, TaskId) {
    let scheduler = SCHEDULER.lock();
    let tasks = scheduler
        .tasks
        .iter()
        .map(|task| (task.id, task.name.clone(), task.state))
        .collect();
    (tasks, scheduler.tasks[scheduler.current].id)
}

// ---------------------------------------------------------------------------
// Demo workload (shell `taskdemo` and the test harness)
// ---------------------------------------------------------------------------

/// Background tasks spawned by the `taskdemo` shell command and reused by the
/// test harness to verify the context switch under load: a demo task printing
/// three numbered lines across yields, a stress task yielding
/// [`STRESS_YIELD_COUNT`] times, a counter task that wakes once per
/// round-robin round until the stress task finishes, and (shell path only) a
/// sleeper task that proves fresh tasks start with the scheduling context's
/// interrupt state by blocking on the IRQ-driven PIT.
pub mod demo {
    use super::{spawn, yield_now};
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

    /// How many times the stress task yields.
    pub const STRESS_YIELD_COUNT: u64 = 100;

    static DEMO_LINES: AtomicU64 = AtomicU64::new(0);
    static STRESS_YIELDS: AtomicU64 = AtomicU64::new(0);
    static COUNTER_ROUNDS: AtomicU64 = AtomicU64::new(0);
    static STRESS_DONE: AtomicBool = AtomicBool::new(false);
    /// IF as the entries saw it: 0 = no entry ran yet, 1 = disabled,
    /// 2 = enabled. All entries record it as their first action; under
    /// deterministic round-robin every entry sees the same state.
    static IF_AT_ENTRY: AtomicU8 = AtomicU8::new(0);

    /// Record the interrupt state this entry started with. First action of
    /// every entry in the suite.
    fn record_entry_if() {
        let enabled = x86_64::instructions::interrupts::are_enabled();
        IF_AT_ENTRY.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
    }

    // Test-only observability: the in-kernel harness asserts on these
    // counters; normal builds never read them.
    /// Yields performed by the stress task so far.
    #[cfg(feature = "test-harness")]
    pub fn stress_yields() -> u64 {
        STRESS_YIELDS.load(Ordering::Relaxed)
    }

    /// Scheduling rounds the counter task observed before the stress task
    /// finished (== stress yield count under deterministic round-robin).
    #[cfg(feature = "test-harness")]
    pub fn counter_rounds() -> u64 {
        COUNTER_ROUNDS.load(Ordering::Relaxed)
    }

    /// Lines printed by the demo task so far.
    #[cfg(feature = "test-harness")]
    pub fn demo_lines() -> u64 {
        DEMO_LINES.load(Ordering::Relaxed)
    }

    /// Whether the stress task has finished its last yield.
    #[cfg(feature = "test-harness")]
    pub fn stress_done() -> bool {
        STRESS_DONE.load(Ordering::Relaxed)
    }

    /// The interrupt state the fresh tasks started with: `Some(false)` when
    /// every entry saw interrupts disabled, `Some(true)` when enabled,
    /// `None` when no entry ran (or the values disagreed, which itself
    /// indicates a start_if bug).
    #[cfg(feature = "test-harness")]
    pub fn entry_interrupts_enabled() -> Option<bool> {
        match IF_AT_ENTRY.load(Ordering::Relaxed) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    /// Spawn the task suite, all Ready: demo, stress, counter, and — on the
    /// shell path only (`include_sleeper`) — a sleeper task that blocks on
    /// the IRQ-driven PIT. The harness must pass `false`: it runs with
    /// interrupts disabled, where `time::sleep_ms` would never wake.
    pub fn spawn_suite(include_sleeper: bool) -> Result<(), &'static str> {
        spawn("demo", demo_entry)?;
        spawn("stress", stress_entry)?;
        spawn("counter", counter_entry)?;
        if include_sleeper {
            spawn("sleeper", sleeper_entry)?;
        }
        Ok(())
    }

    /// Prints three numbered lines, one per scheduling round, then exits by
    /// returning into the trampoline.
    extern "C" fn demo_entry() {
        record_entry_if();
        let id = super::current_id();
        for line in 1..=3 {
            crate::terminal::print_fmt(format_args!(
                "[task] demo #{}: line {} of 3\n",
                id, line
            ));
            DEMO_LINES.fetch_add(1, Ordering::Relaxed);
            yield_now();
        }
        crate::terminal::print("[task] demo: done\n");
    }

    /// Yields [`STRESS_YIELD_COUNT`] times, silently, then finishes. Each
    /// yield forces two full context switches (out via yield, back in via the
    /// scheduler), exercising the switch ABI 200+ times in a row.
    extern "C" fn stress_entry() {
        record_entry_if();
        for _ in 0..STRESS_YIELD_COUNT {
            STRESS_YIELDS.fetch_add(1, Ordering::Relaxed);
            yield_now();
        }
        STRESS_DONE.store(true, Ordering::Relaxed);
    }

    /// Wakes once per round-robin round and counts the round, until the
    /// stress task is done; then reports and exits. Verifies that repeated
    /// switches land back on the correct stack and frame every time.
    extern "C" fn counter_entry() {
        record_entry_if();
        let mut rounds = 0u64;
        while !STRESS_DONE.load(Ordering::Relaxed) {
            rounds += 1;
            COUNTER_ROUNDS.store(rounds, Ordering::Relaxed);
            yield_now();
        }
        crate::terminal::print_fmt(format_args!(
            "[task] counter: stress finished after {} rounds ({} yields exercised the switch)\n",
            rounds,
            STRESS_YIELDS.load(Ordering::Relaxed)
        ));
    }

    /// Sleeps on the IRQ-driven PIT (`time::sleep_ms` parks in `enable_and_hlt`
    /// and only a timer interrupt can wake it), then reports the elapsed
    /// ticks. Only spawned on the shell path: with interrupts disabled this
    /// would sleep forever, which is exactly what makes it the live proof
    /// that fresh tasks inherit the scheduling context's interrupt state.
    extern "C" fn sleeper_entry() {
        record_entry_if();
        let before = crate::time::ticks();
        crate::time::sleep_ms(250);
        let elapsed_ms = (crate::time::ticks() - before) * 10;
        crate::terminal::print_fmt(format_args!(
            "[task] sleeper: IRQ-driven sleep returned after ~{} ms\n",
            elapsed_ms
        ));
    }
}
