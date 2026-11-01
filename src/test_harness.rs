//! Automated kernel test harness (cargo feature `test-harness`).
//!
//! Built with `--features test-harness` and run under QEMU with
//! `-device isa-debug-exit,iobase=0xf4,iosize=0x04` (see `make test`).
//! `kernel_main` calls [`run`] after the filesystem is up and before
//! interrupts are enabled or the shell starts, so the harness never
//! touches the scancode queue, the shell, or any IRQ path.
//!
//! Every test prints `[PASS]`/`[FAIL]` to the terminal (mirrored to COM1)
//! and returns a `Result`, so one failure never halts the rest. When all
//! tests are done, QEMU is exited through the isa-debug-exit port:
//!   - all passed:  0x10 -> QEMU exit status (0x10 << 1) | 1 = 33
//!   - any failure: 0x11 -> QEMU exit status (0x11 << 1) | 1 = 35
//!
//! Tests are deterministic, clean up what they create, and require no
//! input from the host (no keyboard, no QMP).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// QEMU isa-debug-exit I/O port (matches `-device iobase=0xf4`).
const DEBUG_EXIT_PORT: u16 = 0xF4;
/// Written on success; QEMU exits with `(0x10 << 1) | 1` = 33.
const EXIT_PASS: u8 = 0x10;
/// Written when any test failed; QEMU exits with `(0x11 << 1) | 1` = 35.
const EXIT_FAIL: u8 = 0x11;

const HARNESS_FILE: &str = "/harness.txt";
const HARNESS_DIR: &str = "/harness-dir";
const HARNESS_DIR_FILE: &str = "/harness-dir/inner.txt";

type TestFn = fn() -> Result<(), String>;

/// (name, function) pairs, run in order. `fs_crud` must run before
/// `path_normalization`: both use `/harness-dir` and each cleans up after
/// itself. `keyboard_decode` and `framebuffer_format` are pure tests with
/// no resource or ordering constraints.
static TESTS: [(&str, TestFn); 7] = [
    ("heap_churn", test_heap_churn),
    ("frame_allocator", test_frame_allocator),
    ("fs_crud", test_fs_crud),
    ("path_normalization", test_path_normalization),
    ("error_formatting", test_error_formatting),
    ("keyboard_decode", test_keyboard_decode),
    ("framebuffer_format", test_framebuffer_format),
];

/// Run all tests and exit QEMU with the aggregate status. Diverges.
pub fn run() -> ! {
    crate::terminal::print("\n=== kernel test harness ===\n");

    let mut failed = 0usize;
    for (name, test) in TESTS.iter() {
        match test() {
            Ok(()) => crate::terminal::print(&format!("[PASS] {}\n", name)),
            Err(msg) => {
                failed += 1;
                crate::terminal::print(&format!("[FAIL] {} ({})\n", name, msg));
            }
        }
    }

    crate::terminal::print(&format!("\n{} test(s), {} failed\n", TESTS.len(), failed));

    if failed == 0 {
        crate::terminal::print("ALL TESTS PASSED\n");
        qemu_exit(EXIT_PASS);
    }
    crate::terminal::print("TESTS FAILED\n");
    qemu_exit(EXIT_FAIL);
}

/// Write `code` to the isa-debug-exit port so QEMU exits with
/// `(code << 1) | 1`. Interrupts are still disabled at this point; if the
/// device is absent (real hardware) we simply halt forever.
fn qemu_exit(code: u8) -> ! {
    let mut port = Port::new(DEBUG_EXIT_PORT);
    unsafe {
        port.write(code);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

/// Heap churn: repeatedly allocate, verify, and drop `Vec`s and `String`s
/// of varied sizes while other allocations stay live, forcing the
/// linked-list allocator to reuse and coalesce freed blocks. Contents are
/// re-checked after later allocations so a clobbered block is caught.
fn test_heap_churn() -> Result<(), String> {
    const SIZES: [usize; 6] = [16, 64, 777, 1024, 4096, 16_384];
    const ROUNDS: usize = 8;
    const STRINGS_PER_ROUND: u32 = 16;

    for round in 0..ROUNDS {
        let mut live: Vec<Vec<u8>> = Vec::new();
        for (i, &size) in SIZES.iter().enumerate() {
            let mut v = Vec::with_capacity(size);
            for j in 0..size {
                v.push(((i * 31 + j * 7 + round) % 256) as u8);
            }
            live.push(v);
        }

        // All allocations above are live simultaneously; verify each one
        // still holds the exact bytes written.
        for (i, v) in live.iter().enumerate() {
            let size = SIZES[i];
            if v.len() != size {
                return Err(format!(
                    "round {}: alloc {} has len {}, expected {}",
                    round, i, v.len(), size
                ));
            }
            for (j, &byte) in v.iter().enumerate() {
                let expected = ((i * 31 + j * 7 + round) % 256) as u8;
                if byte != expected {
                    return Err(format!(
                        "round {}: alloc {} byte {} corrupted",
                        round, i, j
                    ));
                }
            }
        }

        // String traffic while the Vecs stay live, then verify.
        let mut strings: Vec<String> = Vec::new();
        for k in 0..STRINGS_PER_ROUND {
            strings.push(format!("harness-{:02}-{}", k, "x".repeat((k as usize % 64) + 1)));
        }
        for (k, s) in strings.iter().enumerate() {
            let prefix = format!("harness-{:02}-", k);
            if !s.starts_with(&prefix) || s.len() != prefix.len() + (k % 64) + 1 {
                return Err(format!("round {}: string {} has unexpected contents", round, k));
            }
        }

        drop(strings);
        drop(live);
    }
    Ok(())
}

/// Frame allocator: allocate three frames, check 4 KiB alignment,
/// distinctness, and the < 4 GiB window, free them again, and verify the
/// free count returns to its starting value. (Proving exhaustion behaviour
/// would need to allocate every free frame, which is not practical here.)
fn test_frame_allocator() -> Result<(), String> {
    const COUNT: usize = 3;
    let mut allocator = crate::memory::FRAME_ALLOCATOR.lock();
    let before = allocator.free_frame_count();
    if before < COUNT as u64 {
        return Err(format!("only {} free frames, need {}", before, COUNT));
    }

    let mut frames = [0u64; COUNT];
    for slot in frames.iter_mut() {
        match allocator.allocate_frame() {
            Some(addr) => *slot = addr,
            None => {
                return Err("allocate_frame returned None while frames were free".into())
            }
        }
    }

    for (i, &addr) in frames.iter().enumerate() {
        if addr % crate::memory::FRAME_SIZE != 0 {
            return Err(format!("frame {} at {:#x} is not 4 KiB aligned", i, addr));
        }
        if addr >= crate::memory::MAX_SUPPORTED_PHYS {
            return Err(format!("frame {} at {:#x} is outside the 4 GiB window", i, addr));
        }
    }
    for i in 0..COUNT {
        for j in (i + 1)..COUNT {
            if frames[i] == frames[j] {
                return Err(format!("frames {} and {} share address {:#x}", i, j, frames[i]));
            }
        }
    }

    for &addr in frames.iter() {
        allocator.free_frame(addr);
    }

    let after = allocator.free_frame_count();
    if after != before {
        return Err(format!("free count {} -> {} after alloc/free cycle", before, after));
    }
    Ok(())
}

/// Filesystem CRUD: create a file, read it back, overwrite it, read it
/// back, delete it, confirm reads now fail with NotFound; then directory
/// lifecycle: mkdir, file inside, rmdir of the non-empty directory must
/// fail, remove the file, rmdir must succeed.
fn test_fs_crud() -> Result<(), String> {
    const CONTENT_1: &[u8] = b"hello from the ilo-lawa test harness";
    const CONTENT_2: &[u8] = b"overwritten by the second write";

    crate::fs::create_file(HARNESS_FILE, CONTENT_1.to_vec())
        .map_err(|e| format!("create failed: {}", e))?;
    let data = crate::fs::read_file(HARNESS_FILE)
        .map_err(|e| format!("read after create failed: {}", e))?;
    if data != CONTENT_1 {
        return Err(format!(
            "read-back mismatch after create: {} bytes, expected {}",
            data.len(),
            CONTENT_1.len()
        ));
    }

    crate::fs::create_file(HARNESS_FILE, CONTENT_2.to_vec())
        .map_err(|e| format!("overwrite failed: {}", e))?;
    let data = crate::fs::read_file(HARNESS_FILE)
        .map_err(|e| format!("read after overwrite failed: {}", e))?;
    if data != CONTENT_2 {
        return Err(format!(
            "read-back mismatch after overwrite: {} bytes, expected {}",
            data.len(),
            CONTENT_2.len()
        ));
    }

    crate::fs::remove_file(HARNESS_FILE)
        .map_err(|e| format!("rm failed: {}", e))?;
    match crate::fs::read_file(HARNESS_FILE) {
        Err(msg) => {
            if !msg.contains("No such file or directory") {
                return Err(format!("missing-file read gave wrong error: {:?}", msg));
            }
        }
        Ok(data) => {
            return Err(format!("read of deleted file returned {} bytes", data.len()));
        }
    }

    crate::fs::create_directory(HARNESS_DIR)
        .map_err(|e| format!("mkdir failed: {}", e))?;
    crate::fs::create_file(HARNESS_DIR_FILE, b"nested".to_vec())
        .map_err(|e| format!("create inside dir failed: {}", e))?;

    match crate::fs::remove_directory(HARNESS_DIR) {
        Err(_) => {} // expected: rmdir of a non-empty directory must fail
        Ok(()) => return Err("rmdir of non-empty directory succeeded".into()),
    }

    crate::fs::remove_file(HARNESS_DIR_FILE)
        .map_err(|e| format!("rm inside dir failed: {}", e))?;
    crate::fs::remove_directory(HARNESS_DIR)
        .map_err(|e| format!("rmdir after emptying failed: {}", e))?;
    Ok(())
}

/// Path normalization via the shell-level cd/pwd wrappers: trailing and
/// duplicate slashes collapse, `..` cannot escape the root, and removing
/// the current directory is rejected.
fn test_path_normalization() -> Result<(), String> {
    // fs_crud removes /harness-dir; recreate it (AlreadyExists is fine).
    if let Err(e) = crate::fs::create_directory(HARNESS_DIR) {
        if !e.contains("already exist") {
            return Err(format!("mkdir failed: {}", e));
        }
    }

    crate::fs::change_directory(HARNESS_DIR)
        .map_err(|e| format!("cd /harness-dir failed: {}", e))?;
    let pwd = crate::fs::get_current_path();
    if pwd != "/harness-dir" {
        return Err(format!("pwd after cd /harness-dir is {:?}", pwd));
    }

    crate::fs::change_directory("..").map_err(|e| format!("cd .. failed: {}", e))?;
    let pwd = crate::fs::get_current_path();
    if pwd != "/" {
        return Err(format!("pwd after cd .. is {:?}", pwd));
    }

    crate::fs::change_directory("//harness-dir//")
        .map_err(|e| format!("cd //harness-dir// failed: {}", e))?;
    let pwd = crate::fs::get_current_path();
    if pwd != "/harness-dir" {
        return Err(format!("pwd after cd //harness-dir// is {:?}", pwd));
    }

    crate::fs::change_directory("/../../")
        .map_err(|e| format!("cd /../../ failed: {}", e))?;
    let pwd = crate::fs::get_current_path();
    if pwd != "/" {
        return Err(format!("pwd after cd /../../ is {:?}", pwd));
    }

    // rmdir of the current directory must be rejected.
    crate::fs::change_directory(HARNESS_DIR)
        .map_err(|e| format!("cd back into harness dir failed: {}", e))?;
    match crate::fs::remove_directory(HARNESS_DIR) {
        Err(_) => {} // expected
        Ok(()) => return Err("rmdir of the current directory succeeded".into()),
    }

    // Clean up for a possible rerun / later tests.
    crate::fs::change_directory("/").map_err(|e| format!("cd / failed: {}", e))?;
    crate::fs::remove_directory(HARNESS_DIR)
        .map_err(|e| format!("cleanup rmdir failed: {}", e))?;
    Ok(())
}

/// Error formatting: operations on missing paths must return a non-empty
/// message that includes the offending path.
fn test_error_formatting() -> Result<(), String> {
    let missing_file = "/definitely-missing-harness.txt";
    match crate::fs::read_file(missing_file) {
        Err(msg) => {
            if msg.is_empty() {
                return Err("read_file returned an empty error message".into());
            }
            if !msg.contains(missing_file) {
                return Err(format!("error message does not mention the path: {:?}", msg));
            }
        }
        Ok(data) => {
            return Err(format!("read of missing file returned {} bytes", data.len()));
        }
    }

    let missing_dir = "/definitely-missing-harness-dir";
    match crate::fs::list_directory(missing_dir) {
        Err(msg) => {
            if msg.is_empty() {
                return Err("list_directory returned an empty error message".into());
            }
            if !msg.contains(missing_dir) {
                return Err(format!("error message does not mention the path: {:?}", msg));
            }
        }
        Ok(entries) => {
            return Err(format!("list of missing dir returned {} entries", entries.len()));
        }
    }
    Ok(())
}

/// Keyboard decode state machine: extended (0xE0-prefixed) and plain
/// scancodes must map to the right KeyEvents, fake shifts must not corrupt
/// the modifier state, and Ctrl combos must resolve to CtrlC/CtrlL. Pure
/// `Keyboard` state, no queue, shell, or IRQ involvement.
fn test_keyboard_decode() -> Result<(), String> {
    use crate::keyboard::{Keyboard, KeyEvent};

    // Feed `scancodes` to a fresh Keyboard and require that exactly the
    // expected events emerge (`None` = scancodes produce no event).
    let expect = |scancodes: &[u8], expected: Option<KeyEvent>, desc: &str| {
        let mut kb = Keyboard::new();
        let mut events: Vec<KeyEvent> = Vec::new();
        for sc in scancodes {
            if let Some(event) = kb.process_scancode(*sc) {
                events.push(event);
            }
        }
        let matched = match (&events[..], &expected) {
            ([], None) => true,
            ([event], Some(expected)) => event == expected,
            _ => false,
        };
        if matched {
            Ok(())
        } else {
            Err(format!(
                "{}: got {:?}, expected {:?}",
                desc, events, expected
            ))
        }
    };

    // Extended prefix decoding (arrows, Home/End, Delete, keypad Enter).
    expect(&[0xE0, 0x48], Some(KeyEvent::Up), "E0 48")?;
    expect(&[0xE0, 0x50], Some(KeyEvent::Down), "E0 50")?;
    expect(&[0xE0, 0x4B], Some(KeyEvent::Left), "E0 4B")?;
    expect(&[0xE0, 0x4D], Some(KeyEvent::Right), "E0 4D")?;
    expect(&[0xE0, 0x47], Some(KeyEvent::Home), "E0 47")?;
    expect(&[0xE0, 0x4F], Some(KeyEvent::End), "E0 4F")?;
    expect(&[0xE0, 0x53], Some(KeyEvent::Delete), "E0 53")?;
    expect(&[0xE0, 0x1C], Some(KeyEvent::Enter), "E0 1C")?;

    // Plain (non-extended) navigation and editing keys.
    expect(&[0x48], Some(KeyEvent::Up), "48")?;
    expect(&[0x4B], Some(KeyEvent::Left), "4B")?;
    expect(&[0x53], Some(KeyEvent::Delete), "53")?;
    expect(&[0x1C], Some(KeyEvent::Enter), "1C")?;
    expect(&[0x0E], Some(KeyEvent::Backspace), "0E")?;

    // Fake shifts (E0 2A before, E0 AA after an extended key) must be
    // ignored AND must not set the shift state: plain 'a' after them.
    expect(&[0xE0, 0x2A, 0x1E], Some(KeyEvent::Char('a')), "E0 2A 1E")?;
    expect(
        &[0xE0, 0x2A, 0xE0, 0xAA, 0x1E],
        Some(KeyEvent::Char('a')),
        "E0 2A E0 AA 1E",
    )?;
    expect(&[0xE0, 0xAA], None, "E0 AA ignored")?;
    // ...while the real shift still applies: 2A 1E gives 'A'.
    expect(&[0x2A, 0x1E], Some(KeyEvent::Char('A')), "2A 1E shift")?;

    // Extended release codes are ignored.
    for sc in [0xC8u8, 0xD0, 0xCB, 0xCD, 0xC7, 0xCF, 0xD3, 0x9C] {
        expect(&[0xE0, sc], None, "E0 release")?;
    }

    // Ctrl+C / Ctrl+L; Ctrl release must clear the modifier.
    expect(&[0x1D, 0x2E], Some(KeyEvent::CtrlC), "ctrl c")?;
    expect(&[0x1D, 0x26], Some(KeyEvent::CtrlL), "ctrl l")?;
    expect(
        &[0x1D, 0x9D, 0x2E],
        Some(KeyEvent::Char('c')),
        "ctrl release",
    )?;
    // Right Ctrl (extended) tracks the same modifier.
    expect(&[0xE0, 0x1D, 0x2E], Some(KeyEvent::CtrlC), "right ctrl c")?;
    expect(
        &[0xE0, 0x1D, 0xE0, 0x9D, 0x2E],
        Some(KeyEvent::Char('c')),
        "right ctrl release",
    )?;
    // Ctrl with an unbound character produces no event.
    expect(&[0x1D, 0x1E], None, "ctrl a unbound")?;

    Ok(())
}

/// Pixel-format encoding (issue #11): a 0x00RRGGBB color must map to the
/// exact native u32 each framebuffer layout expects. Pure functions on
/// `kernel::PixelFormat` — no framebuffer, GOP, or hardware dependency.
fn test_framebuffer_format() -> Result<(), String> {
    use crate::kernel::{PixelFormat, encode_pixel};

    const GREEN: u32 = 0x00FF00;
    const RED: u32 = 0xFF0000;
    const BLUE: u32 = 0x0000FF;
    const WHITE: u32 = 0xFFFFFF;

    // Bgr (QEMU's default GOP format): the value is written unchanged.
    let bgr = PixelFormat::Bgr;
    for (rgb, want) in [(GREEN, GREEN), (RED, RED), (BLUE, BLUE), (WHITE, WHITE)] {
        let got = encode_pixel(&bgr, rgb);
        if got != want {
            return Err(format!("Bgr {rgb:#08x}: got {got:#08x}, want {want:#08x}"));
        }
    }

    // Rgb: red and blue are swapped, green stays in place.
    let rgb = PixelFormat::Rgb;
    for (rgb_in, want) in [
        (GREEN, GREEN),
        (RED, BLUE),
        (BLUE, RED),
        (WHITE, WHITE),
    ] {
        let got = encode_pixel(&rgb, rgb_in);
        if got != want {
            return Err(format!("Rgb {rgb_in:#08x}: got {got:#08x}, want {want:#08x}"));
        }
    }

    // Bitmask with full 8-bit channels (plus reserved bits at the top):
    // must behave exactly like Bgr.
    let mask32 = PixelFormat::Bitmask {
        red_mask: 0x00FF_0000,
        green_mask: 0x0000_FF00,
        blue_mask: 0x0000_00FF,
    };
    for (rgb_in, want) in [(GREEN, GREEN), (RED, RED), (BLUE, BLUE), (WHITE, WHITE)] {
        let got = encode_pixel(&mask32, rgb_in);
        if got != want {
            return Err(format!("Bitmask32 {rgb_in:#08x}: got {got:#08x}, want {want:#08x}"));
        }
    }

    // Bitmask with non-8-bit channels, 16 bpp 5/6/5: each channel is scaled
    // from 0..=255 to the full mask width and aligned to its lowest set bit.
    let mask565 = PixelFormat::Bitmask {
        red_mask: 0xF800,
        green_mask: 0x07E0,
        blue_mask: 0x001F,
    };
    for (rgb_in, want) in [
        (RED, 31 << 11),
        (GREEN, 63 << 5),
        (BLUE, 31),
        (WHITE, 0xFFFF),
    ] {
        let got = encode_pixel(&mask565, rgb_in);
        if got != want {
            return Err(format!("Bitmask565 {rgb_in:#08x}: got {got:#08x}, want {want:#08x}"));
        }
    }

    // Zero-width masks are safe and contribute nothing.
    let mask_zero = PixelFormat::Bitmask {
        red_mask: 0,
        green_mask: 0x0000_FF00,
        blue_mask: 0,
    };
    let got = encode_pixel(&mask_zero, WHITE);
    if got != 0x0000_FF00 {
        return Err(format!("Bitmask zero-width: got {got:#08x}, want 0x0000ff00"));
    }

    Ok(())
}
