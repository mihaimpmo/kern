# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Kern is a small monolithic x86-64 kernel in Rust (`no_std`), booted by Limine, with ring-3 processes loaded from ELF files. `README.md` covers features, hardware boot and the roadmap.

## Commands

```sh
make            # build kernel + user programs into kern.iso (fetches and builds limine/ on first run)
make run        # QEMU, legacy BIOS, serial on stdio, disk.img as an NVMe SSD at /disk (made on first use)
make run-uefi   # QEMU, UEFI (needs OVMF)
make test       # selftest on 1 and 4 CPUs, two CPU models, with and without a disk, a two-boot
                # persistence test, and a scripted shell session (~2-3 min)
make clean      # remove the ISO and cargo artifacts; distclean also removes limine/

cd kernel && cargo check    # fast type-check of the kernel
cd user && cargo check      # same for all user programs
```

`kernel/` and `user/` are separate cargo projects, so run cargo from inside one of them, not the repo root. Each has a `.cargo/config.toml` that sets the target and `build-std`. The kernel uses `x86_64-unknown-none` (soft-float: the kernel never touches FPU/SSE registers). User programs use Kern's own `user/x86_64-kern-user.json`, which is the same but with SSE floating point, so their binaries land in `user/target/x86_64-kern-user/release/`.

The toolchain is pinned to nightly in `rust-toolchain.toml`. The README's note about stable plus `RUSTC_BOOTSTRAP=1` describes what the Makefile exports, but the pin takes effect here.

### Testing

`make test` runs `tests/run.py`, which has three parts:
- **selftest** (`user/selftest`): a user program that checks kernel behaviour and prints TAP-style `ok`/`not ok` lines. `kern-test.iso` boots it as pid 1 (`cmdline: init=selftest qemu-exit`); when it exits, the kernel writes its exit code (the number of failures) to QEMU's isa-debug-exit device, so QEMU's exit status is the verdict. It runs on `-cpu qemu64` (no SMEP/SMAP) and `-cpu max`, with 1 and 4 CPUs (`CONFIGS` in `tests/run.py`; several vCPUs use multi-threaded TCG so they really run in parallel), and the runner checks the boot log reports the expected CPU count. Add new kernel behaviour as checks here. Helper child processes are copies of selftest started in a mode (`selftest exit 3`, `selftest fault`, ...).
- **disk**: the disk runs (and the shell test) get a fresh 32 MiB KFS image made by `tools/kfs.py mkfs`; after each run `tools/kfs.py check` must find it consistent, and a persistence run boots twice on one image (selftest's boot counter in `/disk/boots`). The generic file checks in selftest (`file_suite`) run on both `/tmp` and `/disk`; `disk()` adds what only the disk does.
- **shell**: boots `kern.iso` and types into `init` through the QEMU monitor's `sendkey` (serial is output-only; input comes only from the PS/2 keyboard), waiting for regexes in the serial log. Steps are `SHELL_STEPS` in `tests/run.py`.

`cargo test` is not set up for these bare-metal targets. The console mirrors to serial, including panics, so for a manual headless boot send serial to a file:

```sh
timeout --foreground 15 qemu-system-x86_64 -M q35 -m 512M -cdrom kern.iso \
    -serial file:serial.log -display none -no-reboot </dev/null
```

## Architecture

**Boot flow:** `kmain` in `kernel/src/main.rs` runs subsystem init in a dependency-sensitive order (the header comment lists it). Heap allocation is unavailable before `mm::heap::init`. The boot thread ends up as the idle task (pid 0). Pid 1 is `init` unless the kernel command line (`cmdline.rs`) says `init=<program>`; when pid 1 exits the system halts (`power.rs`).

**Files** (`kernel/src/fs/`): one in-memory tree (`tmpfs`) rooted at `/`, with `/dev/console`, `/dev/null` and `/tmp`. At boot the kernel unpacks the initramfs, a ustar archive the Makefile builds from every program into `/bin` and Limine loads as `/boot/initrd.tar`. Paths are canonicalised textually (`fs::canonical`: `.`/`..` folded, no symlinks). `OpenFile`s (inode + offset) are shared by `dup` and inherited by spawned children, which is how the shell redirects without `fork`. A new program must be registered in both of these, or it won't be in `/bin`:
- `members` in `user/Cargo.toml`
- `PROGRAMS` in the `Makefile`

Each program also needs a `build.rs` that passes `user/linker.ld`; copy `user/hello`. Programs get their arguments through `ulib::args()`: `task/exec.rs` puts argc/argv on the new stack in the System V layout, and `_start` in `ulib` hands that pointer to `ulib_start`.

**Storage:** `drivers/pci.rs` enumerates PCI through the legacy config ports; `drivers/nvme.rs` drives NVMe SSDs (one I/O queue, one command in flight under a `Mutex`, polled completions, one bounce page, so every transfer is one 4 KiB block). Disks implement `block::BlockDevice` and are registered at boot. `fs/kfs.rs` is KFS, Kern's on-disk filesystem, mounted at `/disk` (`/disk1`, ...) through a `block::Cached` write-through cache; its format is specified in the header of `tools/kfs.py`, the host tool (`mkfs`, `check`, `ls`, `cat`, `put`), and the two must agree, so change both together. A KFS mount takes one `Mutex` for every operation, disk I/O included. KFS code works with inode numbers internally and must never drop a `Node` while holding that lock (`Node::drop` takes it, to free unlinked files on their last close). The kernel only mounts disks whose first block has the KFS magic and never formats one: on real hardware it must not write to a disk it doesn't own.

**Syscalls span both trees.** The numbers are defined independently in `syscall_dispatch` (`kernel/src/syscall/mod.rs`, with the handlers in `file.rs`, `process.rs` and `memory.rs` next to it) and `ulib::sys` (`user/ulib/src/lib.rs`), so change both together, along with the table in the `syscall/mod.rs` header comment. Handlers return `Result<u64, Errno>`; errno and flag values are Linux's. User memory must only be accessed through `copy_from_user` / `copy_to_user` (or checked first with `check_user`): they validate the pages and open the SMAP window (`stac`/`clac`), so any other access to a user pointer faults in the kernel on CPUs with SMAP. Exit statuses go through a pointer (`wait`, `try_wait`): `ExitStatus::encode` puts an exit code in the low 32 bits, or `1 << 32 | signal` for a killed process. Arguments are rdi, rsi, rdx, r10, r8, r9 (`ulib::syscall6`). `ulib` wrappers return `Result<_, Errno>`.

**SMP:** every CPU runs the kernel. `arch/smp.rs` starts the others through Limine's MP request; each repeats the per-CPU part of boot (`ap_entry`): its own GDT/TSS and IST stacks and GS base (`percpu::init`), the shared IDT, CR4 protections, local APIC and timer, syscall MSRs, then its own idle task. Machine-wide timer chores (uptime, sleepers, draining serial, NMI reports) run only on CPU 0. A panic, or pid 1 exiting, NMIs the other CPUs to a halt (`power.rs`).

**Scheduling and locking:**
- All scheduler state is in one `SpinLock` (`SCHED` in `task/mod.rs`) with a single ready queue shared by all CPUs. The lock is held across `switch_context` and released by the task that resumes (by dropping its own guard, or `force_unlock` in `task_start` for a new task). Code inside the scheduler must not print or take locks that are held while taking `SCHED`.
- `SpinLock` also disables interrupts on the local CPU while held, so it is safe to share with interrupt handlers. Disabling interrupts alone protects nothing any more, since other CPUs keep running.
- Blocking uses `WaitQueue` like a condition variable: check the condition under its lock, then `queue.wait(guard)`, in a loop. Lock order is caller's lock, then `SCHED`, then the queue's own lock; never call `wake_all` with `SCHED` held.
- `percpu::this()` / `percpu::index()` are only meaningful with interrupts off, because a preempted task may resume on another CPU.
- `BootCell<T>` is for globals written only during single-threaded early boot (the IDT). There is no unsynchronised cell for anything else.
- Kernel stacks are pooled, never unmapped (`mm/kstack.rs`): unmapping kernel memory would need a TLB shootdown across CPUs, which doesn't exist yet.
- Syscalls run with interrupts enabled, so they can be preempted.
- Serial output is buffered (`drivers/serial.rs`): writes go to a ring that `serial::poll` drains from the timer interrupt. It is synchronous until `start_buffering` (just before the idle loop) and again after a panic, so early-boot hangs and panics still reach the log.
- NMIs can interrupt code that holds any lock, so the NMI handler only counts; the timer prints the report.

**Processes:** a user task's `Process` (`task/mod.rs`) holds its `UserSpace`, `FdTable` and working directory, each behind its own `SpinLock`. File operations can block (console input, a `Mutex`), so syscalls clone what they need out of those locks before calling into `fs` (see the note at the top of `syscall/file.rs`). `sync::Mutex` sleeps instead of spinning and is for long or blocking critical sections; never take it in an interrupt handler.

**Process lifetime:** `exit_current` closes the task's files (outside the scheduler lock, since closing can wake other tasks) and marks it a zombie, because it can't free the stack it is running on. `reap` (called from `schedule`) later frees its kernel stack and address space on another task's stack. The `Task` entry stays until the parent collects the exit code through `wait` / `try_wait_any`, so an entry can outlive its resources. A user-mode fault kills only the process, with a signal (`fault_signal` in `arch/interrupts.rs`: SIGSEGV, SIGILL, SIGFPE, ...); a kernel-mode fault panics, and so do a double fault or machine check in either mode. An NMI is logged and ignored.

**Signals** have no handlers: every signal kills. `task::kill` sets `pending_signal` and wakes the target if it sleeps or blocks; the target dies in `deliver_signals`, called on every return to user mode (end of `syscall_dispatch`, and `interrupt_dispatch` when the interrupt came from user mode, so even a process that never makes a system call dies within a timer tick). Waits that a signal should cut short use `WaitQueue::wait_interruptible` (console and pipe reads, pipe writes; `task::wait` and `sleep_ms` check too); it checks for a pending signal under the scheduler lock before blocking, so a kill can't slip in between. `Mutex` waits are not interruptible. Processes have a process group (`setpgid`); the console's foreground group (`keyboard::set_foreground`, set by the shell around each job) gets SIGINT on Ctrl+C, and with no foreground group the key arrives as byte 3. Pid 1 can't be signalled.

**Pipes** (`fs/pipe.rs`): each end is one inode, shared by every descriptor that refers to it, so an end closes when its last descriptor does (`Drop`): reads then see EOF, writes get EPIPE. Descriptors opened with `CLOSE_ON_SPAWN` (O_CLOEXEC; `open` and `pipe` take it) aren't inherited by spawned children (`FdTable::for_child`); `dup`/`dup2` create inheritable ones. The shell relies on this so that only the intended child holds each pipe end.

**FPU:** x87/SSE are enabled on every CPU (`arch/fpu.rs`) and each user task has a 512-byte `fxsave` area, saved and restored eagerly in `schedule`. That is only correct because the kernel itself never uses those registers; keep the kernel soft-float. AVX is not enabled (it would need XSAVE).

**Memory:** the address-space layout is in the header of `kernel/src/mm/mod.rs`. All physical RAM is reachable through the HHDM (`phys_to_virt`). `vmm::init` pre-creates every kernel-half PML4 entry so that later kernel mappings show up in all address spaces automatically; new address spaces share those entries.

**User memory** (`mm/uspace.rs`): a process's `UserSpace` is its page tables plus *regions* (start, end, `Prot`): ELF segments, the heap (`brk`), `mmap` areas and an 8 MiB stack. Pages are allocated on first touch: a not-present user page fault inside a region with the right access maps a zeroed frame (`task::user_page_fault`), and kernel access goes through `populate` first, which is what `copy_from_user`/`copy_to_user` call. The `Task` holds it as `Arc<SpinLock<UserSpace>>`, locked separately from the scheduler, plus a copy of its `pml4` for context switches. Never block or exit while holding that lock or an `Arc` from `task::current_space()` (a reference left on a dead task's stack is never dropped, so its memory would leak). TLB flushes are local only, which is correct because a process's page tables are loaded only on the CPU running it; threads would need shootdowns. In user space, `ulib::heap` is the global allocator (on `brk`), so programs can `extern crate alloc` and use `Vec`/`String`/`Box`.

## Layouts that assembly depends on

Inline assembly and Rust share these with nothing to check them, so a change on one side breaks the other at runtime rather than at compile time:

- `SyscallFrame` must mirror the push order in `syscall_entry` (`syscall.rs`).
- `InterruptFrame` must mirror the pushes in `isr_common` (`arch/interrupts.rs`) and the stubs generated by `kernel/build.rs`.
- `PerCpu` field offsets (`arch/percpu.rs`) are hard-coded as `gs:[0]` and `gs:[8]` in `syscall_entry`, and `gs:[16]` in `percpu::this`.
- `prepare_stack` must match the pop order in `switch_context` (`task/mod.rs`).
- `MpInfo` / `MpResponse` (`boot.rs`) mirror the Limine protocol structs, which the APs' start-up depends on.
- GDT selector order (`arch/gdt.rs`) is constrained by `sysret` through the `STAR` setup in `syscall::init`, and `enter_user` hard-codes `0x1b` and `0x23`. The RPL bits on the sysret base are deliberate: AMD and Intel behave differently (see the comment there).
