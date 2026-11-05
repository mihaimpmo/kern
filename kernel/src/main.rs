//! Kern: a small monolithic x86-64 kernel.
//!
//! Boot order: serial -> framebuffer console -> boot GDT + IDT -> FPU ->
//! SMEP/SMAP/UMIP -> physical memory -> paging -> heap -> per-CPU GDT/TSS ->
//! ACPI/APIC -> timer -> syscalls -> scheduler -> other CPUs -> keyboard ->
//! first user program (`init`) -> buffered serial output -> idle loop.
//! The other CPUs repeat the per-CPU steps in `arch::smp::ap_entry`.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod console;
mod acpi;
mod arch;
mod boot;
mod cmdline;
mod drivers;
mod fs;
mod mm;
mod power;
mod sync;
mod syscall;
mod task;

use arch::{apic, cpu, gdt, idt, percpu};
use core::panic::PanicInfo;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[no_mangle]
extern "C" fn kmain() -> ! {
    drivers::serial::init();
    if !boot::BASE_REVISION.is_supported() {
        cpu::halt_forever(); // bootloader too old for this kernel
    }

    let hhdm = boot::HHDM.response().expect("no HHDM from bootloader");
    mm::set_hhdm_offset(hhdm.offset);
    console::init(boot::FRAMEBUFFER.response().and_then(|r| r.first()));

    kprintln!("\x1b[96mKern\x1b[0m kernel v{} (x86-64)", VERSION);
    if let Some(info) = boot::BOOTLOADER_INFO.response() {
        kprintln!("  booted by {} {}", info.name(), info.version());
    }
    let mut brand = [0u8; 48];
    kprintln!("  cpu: {}", cpu::brand_string(&mut brand));
    if let Some((w, h, _)) = console::size() {
        kprintln!("  display: {}x{}", w, h);
    }

    gdt::load_boot_gdt();
    idt::init();

    arch::fpu::init_cpu();
    arch::fpu::init();
    let prot = cpu::enable_protections();
    let flag = |on: bool, name: &'static str| if on { name } else { "" };
    if prot.smep || prot.smap || prot.umip {
        ok(format_args!(
            "user-memory protection:{}{}{}",
            flag(prot.smep, " SMEP"),
            flag(prot.smap, " SMAP"),
            flag(prot.umip, " UMIP")
        ));
    } else {
        kprintln!("\x1b[93m[warn]\x1b[0m CPU has no SMEP/SMAP/UMIP; relying on syscall pointer checks only");
    }

    let memmap = boot::MEMMAP.response().expect("no memory map from bootloader");
    mm::pmm::init(memmap);
    mm::vmm::init();
    mm::heap::init();
    console::enable_shadow_buffer();
    percpu::init(0);
    ok("GDT, TSS and IDT loaded");
    let (free, total) = mm::pmm::stats();
    ok(format_args!("memory: {} MiB usable, {} MiB free", total >> 20, free >> 20));

    let rsdp = boot::RSDP.response().expect("no ACPI RSDP (is this a UEFI/ACPI machine?)");
    let madt = acpi::parse_madt(rsdp.address).expect("ACPI MADT not found");
    apic::init(&madt);
    ok(format_args!(
        "{} APIC, {} I/O APIC(s), {} CPU(s) found",
        if apic::is_x2apic() { "x2" } else { "x" },
        madt.ioapics.len(),
        madt.cpu_count
    ));

    let ticks = apic::start_timer();
    ok(format_args!("timer: 1000 Hz (APIC bus ~{} MHz)", ticks * 16 / 1000));
    match drivers::rtc::init(task::uptime_ms()) {
        Some(seconds) => ok(format_args!("clock: {} s since 1970 (UTC)", seconds)),
        None => kprintln!("\x1b[93m[warn]\x1b[0m real-time clock unreadable; time() will return 0"),
    }

    syscall::init();
    task::add_cpu();
    ok("scheduler and system calls ready");

    let cpus = arch::smp::start_aps();
    ok(format_args!("{} CPU(s) online", cpus));

    if drivers::keyboard::init(&madt) {
        ok("PS/2 keyboard");
    } else {
        kprintln!("\x1b[93m[warn]\x1b[0m no PS/2 keyboard found (USB keyboards are not supported yet)");
    }

    find_disks();
    fs::init();
    mount_disks();
    let mut unpacked = 0;
    for module in boot::MODULES.response().into_iter().flat_map(|r| r.modules()) {
        if module.path().ends_with(".tar") {
            match fs::initramfs::unpack(module.data()) {
                Ok(files) => unpacked += files,
                Err(why) => kprintln!("\x1b[91m[fail]\x1b[0m initramfs {}: {}", module.path(), why),
            }
        }
    }
    ok(format_args!("filesystem: {} file(s) from the initramfs", unpacked));

    let init = cmdline::get("init").unwrap_or("init");
    // Pid 1 starts in process group 1.
    let inherit = task::Inherit { files: fs::FdTable::with_console(), cwd: alloc::string::String::from("/"), pgid: 1 };
    match task::spawn_program(init, &[init.as_bytes()], None, inherit) {
        Ok(pid) => ok(format_args!("started {} (pid {})", init, pid)),
        Err(e) => kprintln!("\x1b[91m[fail]\x1b[0m could not start {}: {}", init, e),
    }
    kprintln!();

    // Interrupts come on in the idle loop, so the timer can drain buffered
    // serial output from here on. Until now it was written synchronously, so
    // a boot that hangs still shows everything up to the hang.
    drivers::serial::start_buffering();

    // This boot thread becomes the idle task.
    task::idle();
}

/// Scan PCI and set up every NVMe SSD as a block device.
fn find_disks() {
    use drivers::{block, nvme, pci};
    let functions = pci::enumerate();
    ok(format_args!("PCI: {} function(s)", functions.len()));
    for f in functions.iter().filter(|f| (f.class, f.subclass, f.prog_if) == (0x01, 0x08, 0x02)) {
        match nvme::probe(f) {
            Ok(disk) => {
                let name = block::BlockDevice::name(&*disk);
                ok(format_args!("{}: NVMe {:04x}:{:04x}, {} MiB", name, f.vendor, f.device_id, disk_mib(&*disk)));
                block::register(disk);
            }
            Err(why) => kprintln!("\x1b[93m[warn]\x1b[0m NVMe at {:02x}:{:02x}.{}: {}", f.bus, f.device, f.function, why),
        }
    }
}

/// Mount every disk that holds a Kern filesystem: /disk, /disk1, ... Other
/// disks are left alone (nothing is ever written to them).
fn mount_disks() {
    let mut mounted = 0;
    for disk in drivers::block::devices() {
        let name = alloc::string::String::from(disk.name());
        match fs::kfs::mount(disk) {
            Ok(root) => {
                let at = if mounted == 0 { alloc::format!("disk") } else { alloc::format!("disk{}", mounted) };
                match fs::mount(&at, root) {
                    Ok(()) => ok(format_args!("{}: Kern filesystem mounted at /{}", name, at)),
                    Err(e) => kprintln!("\x1b[91m[fail]\x1b[0m {}: cannot mount at /{}: {:?}", name, at, e),
                }
                mounted += 1;
            }
            Err(why) => kprintln!("\x1b[93m[warn]\x1b[0m {}: {}; left untouched", name, why),
        }
    }
}

fn disk_mib(disk: &dyn drivers::block::BlockDevice) -> u64 {
    disk.block_count() * drivers::block::BLOCK_SIZE as u64 >> 20
}

fn ok(msg: impl core::fmt::Display) {
    kprintln!("\x1b[92m[ ok ]\x1b[0m {}", msg);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cpu::cli();
    if !power::stop_other_cpus() {
        cpu::halt_forever(); // another CPU is already panicking or halting
    }
    drivers::serial::stop_buffering();
    console::_panic_print(format_args!("\n\x1b[91m*** KERNEL PANIC ***\x1b[0m\n"));
    if let Some(loc) = info.location() {
        console::_panic_print(format_args!("at {}:{}\n", loc.file(), loc.line()));
    }
    console::_panic_print(format_args!("{}\n", info.message()));
    cpu::halt_forever();
}
