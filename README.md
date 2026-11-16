# Kern

A small monolithic x86-64 kernel written in Rust. It boots on real PCs (UEFI or legacy BIOS) through the [Limine](https://github.com/limine-bootloader/limine) bootloader, runs programs in user mode with memory protection, and switches between them preemptively.

```
[ ok ] GDT, TSS and IDT loaded
[ ok ] memory: 2001 MiB usable, 1995 MiB free
[ ok ] x APIC, 1 I/O APIC(s), 4 CPU(s) found
[ ok ] timer: 1000 Hz (APIC bus ~1001 MHz)
[ ok ] scheduler and system calls ready
[ ok ] 4 CPU(s) online
[ ok ] PS/2 keyboard
[ ok ] PCI: 7 function(s)
[ ok ] nvme0n1: NVMe 1b36:0010, 64 MiB
[ ok ] filesystem: 17 file(s) from the initramfs
[ ok ] nvme0n1: Kern filesystem mounted at /disk
[ ok ] started init (pid 1)

kern:/> hello &
[2] started in background
kern:/> crash
[kernel] process 3 killed: Page fault at 0x400012 (address 0xffffffff80000000)
[3] killed by SIGSEGV
kern:/> ls /bin | wc -l
15
```

## What it has

| Area | Implementation |
|---|---|
| Boot | Limine protocol (base revision 5), one hybrid ISO for UEFI + BIOS |
| CPU setup | GDT, TSS, IDT, 256 interrupt stubs, separate stacks (IST) for double fault / NMI / machine check |
| Hardening | SMEP, SMAP and UMIP when the CPU has them; user memory is only touched through checked copies |
| Physical memory | Free-list frame allocator built from the firmware memory map |
| Virtual memory | 4-level paging, one address space per process, shared kernel half, NX |
| Kernel heap | Grows on demand (`Vec`, `Box`, `String` all work in the kernel) |
| Kernel stacks | 32 KiB each, with an unmapped guard page below to catch overflows |
| Interrupts | Local APIC (xAPIC and x2APIC) + I/O APIC, routes taken from ACPI MADT |
| Timer | APIC timer calibrated against the PIT, 1000 Hz |
| Multi-core | All CPUs run the kernel (Limine MP request), per-CPU GDT/TSS/idle task, panics stop every CPU |
| Scheduler | Preemptive round-robin across all CPUs, 10 ms time slices, sleep, wait queues |
| User mode | Ring 3 processes loaded from ELF files, `syscall`/`sysret`, arguments (argv) |
| Floating point | SSE in user programs, register state saved per task |
| Files | VFS with an in-memory root (`/bin` from an initramfs, `/tmp`, `/dev`), file descriptors, working directory, redirection, rename |
| Storage | PCI enumeration, NVMe driver, write-through block cache; **KFS**, Kern's own on-disk filesystem, mounted at `/disk` |
| User memory | Demand-paged regions: `brk`, `mmap`/`munmap`/`mprotect`, 8 MiB stack; `Vec`/`String`/`Box` in user programs |
| Syscalls | 40: processes (spawn with argv, exec, wait, kill, process groups), signals (sigaction, sigprocmask, sigsuspend), memory (brk, mmap, ...), files (open, read, write, lseek, stat, readdir, mkdir, unlink, rename, chdir, dup2, pipe, statfs, ...), time |
| Signals | Handlers on the user stack (Linux-style `sigaction`, `sigreturn`), blocking masks, `sigsuspend`, SIGCHLD; `kill`, process groups, Ctrl+C interrupts the foreground job; CPU faults raise SIGSEGV/SIGILL/SIGFPE, which a handler can catch |
| Pipes | 64 KiB pipes with EOF and EPIPE, close-on-spawn descriptors |
| Clock | Date and time from the CMOS real-time clock |
| Drivers | Framebuffer text console (with ANSI colours), PS/2 keyboard, buffered serial port |
| User space | `ulib` (mini standard library); a shell (`init`) with pipelines, redirection, job control and history; `ls`, `cat`, `echo`, `wc`, `mkdir`, `rm`, `ps`, `kill`, `sleep`, `date`; demo programs |

## Project layout

```
kern/
├── Makefile            builds everything into kern.iso
├── limine.conf         boot menu; loads the kernel and the initramfs
├── kernel/
│   ├── build.rs        generates interrupt stubs, passes linker script
│   ├── linker.ld       kernel placed at 0xffffffff80000000
│   └── src/
│       ├── main.rs     kmain: boot sequence
│       ├── boot.rs     Limine requests/responses
│       ├── console.rs  framebuffer + serial console, kprintln!
│       ├── acpi.rs     finds the MADT (CPUs, interrupt controllers)
│       ├── syscall/    syscall entry (assembly), dispatch, file/process/memory calls
│       ├── fs/         VFS, tmpfs, KFS (on disk), pipes, devices, initramfs
│       ├── sync.rs     SpinLock, IrqGuard
│       ├── arch/       cpu, gdt, idt, interrupts, apic, percpu, smp
│       ├── mm/         pmm (frames), vmm (page tables), uspace (process memory), heap, kstack
│       ├── task/       scheduler, context switch, ELF loader
│       └── drivers/    keyboard, serial, rtc, pci, nvme, block (cache)
├── user/
│   ├── linker.ld       user programs start at 0x400000
│   ├── ulib/           syscalls, print!/println!, args, heap allocator, _start, panic handler
│   ├── init/           the shell
│   ├── ls/ cat/ wc/ ...   utilities: ls cat echo wc mkdir rm mv df ps kill sleep date
│   ├── hello/          prints with pauses (run several with `hello &`)
│   ├── spin/           hogs the CPU to show preemption
│   ├── crash/          writes to kernel memory to show protection
│   └── selftest/       kernel checks run by `make test`
├── tools/kfs.py        KFS disk images: mkfs, check, ls, cat, put
└── tests/run.py        boots the test ISOs in QEMU and checks the results
```

## Building

You need Linux (WSL works on Windows), plus:

```sh
# Rust, with the standard library source
rustup component add rust-src
# Debian/Ubuntu
sudo apt install build-essential git xorriso
# optional, to test in a VM
sudo apt install qemu-system-x86 ovmf
```

Then:

```sh
make            # -> kern.iso
make run        # test in QEMU, legacy BIOS (with disk.img as an NVMe SSD at /disk)
make run-uefi   # test in QEMU, UEFI
make test       # automated tests in QEMU (needs python3)
```

The Makefile sets `RUSTC_BOOTSTRAP=1` so a stable Rust toolchain can rebuild `core`/`alloc` for the bare-metal targets. With a nightly toolchain that variable is unnecessary. The kernel is built for `x86_64-unknown-none`; user programs for `user/x86_64-kern-user.json`, the same target with SSE floating point enabled.

## The disk

`make run` attaches `disk.img` (created on first use, 64 MiB) as an NVMe SSD; Kern mounts it at `/disk`, and whatever you put there survives a reboot. Delete `disk.img` to start over. `tools/kfs.py` works with images from the host:

```sh
python3 tools/kfs.py mkfs my.img 128        # a new 128 MiB disk
python3 tools/kfs.py put my.img notes.txt /notes.txt
python3 tools/kfs.py ls my.img /
python3 tools/kfs.py cat my.img /notes.txt
python3 tools/kfs.py check my.img           # verify the whole structure
```

KFS uses 4 KiB blocks, a superblock, inode and block bitmaps, 128-byte inodes (12 direct, one indirect and one double-indirect block pointer: files up to ~4 GiB) and directories of 256-byte entries (names up to 248 bytes). The full format is in the header of `tools/kfs.py`. Every write reaches the disk before the call returns; there is no journal yet, so a crash in the middle of an operation can leak blocks, which `kfs.py check` reports.

## Booting on a real computer

1. Write `kern.iso` to a USB stick. It erases the stick.
   - Windows: **Rufus**, and when asked choose **"Write in DD Image mode"**. balenaEtcher also works.
   - Linux: `sudo dd if=kern.iso of=/dev/sdX bs=4M status=progress && sync`
2. In the firmware setup, **turn off Secure Boot**. Limine isn't signed with Microsoft's key.
3. Boot from the USB stick. On Lenovo IdeaPads: press **F12** at power-on, or use the **Novo** button, then choose the USB drive.

Nothing is written to the internal disk. The kernel does drive NVMe SSDs, but it only mounts (and writes to) a disk that already holds a Kern filesystem, recognised by the magic in its first block. It never formats anything, so a disk with Windows or Linux on it is left alone.

### What to expect on real hardware

- **Keyboard:** laptop built-in keyboards are PS/2 internally and work. A USB keyboard usually won't, since there is no USB driver yet. Some firmwares emulate USB keyboards as PS/2 in BIOS (CSM) mode.
- **Screen:** the console runs at the resolution the firmware chose; text is scaled up on screens ≥1600 px wide.
- **CPU:** every core runs (up to 64).
- **Untested path:** if the firmware leaves the CPU in x2APIC mode (common on recent machines), the kernel uses the x2APIC MSR interface. QEMU here couldn't emulate that mode, so this is the first thing to suspect if the boot stops right after the "memory" line.
- If anything goes wrong, the kernel prints a red `KERNEL PANIC` with the file, line and register dump. A photo of that screen is enough to debug it.

## Extending it

**Add a user program:** copy `user/hello`, then add it to `members` in `user/Cargo.toml` and `PROGRAMS` in the Makefile. It ends up in `/bin`, and the shell can run it by name, with arguments (`ulib::args()`).

**Add a system call:** add a number in `syscall_dispatch` (`kernel/src/syscall/mod.rs`) and a handler in `file.rs`, `process.rs` or `memory.rs` next to it, plus a wrapper in `user/ulib`. Only touch user memory through `copy_from_user` / `copy_to_user`; with SMAP on, anything else faults. Add a check for it to `user/selftest` and run `make test`.

**Add a kernel thread:** `task::spawn_kernel("name", function, arg)`.

**Add an interrupt:** pick a vector in `arch/interrupts.rs`, handle it in `interrupt_dispatch`, and route the device's IRQ with `apic::route_isa_irq` (or MSI for PCI devices).

### Suggested next steps, roughly in order

1. **SMP, part 2:** per-CPU run queues instead of one shared queue, wake-up IPIs, TLB shootdown.
2. **PCI enumeration** through ACPI MCFG; it's the gateway to every modern device.
3. **Storage, part 2:** interrupt-driven NVMe with several commands in flight, AHCI for SATA disks, partitions (GPT), a journal for KFS, and FAT32 for exchanging files.
4. **Threads** (they need the TLB shootdown from step 1), then a real libc port.
5. **USB:** xHCI + HID keyboard.
6. Reclaim bootloader memory, a buddy allocator for contiguous frames (needed for DMA).

## Design notes

- **Monolithic:** drivers run inside the kernel, as in Linux.
- **Address space:** user space is `0x0`–`0x7fff_ffff_ffff`; the kernel half (`0xffff_8000_...` and up) is shared by all processes and not accessible from ring 3.
- **Locking:** `SpinLock` disables interrupts while held, so interrupt handlers can't deadlock against the code they interrupted. The scheduler is one spinlock around a shared ready queue, held across the context switch and released by the task that resumes.
- **Process exit:** a process can't free the stack it's running on, so exit marks it a zombie and the scheduler frees its memory on the next switch. The exit status (code, or the signal that killed it) is kept until the parent collects it with `wait`.
