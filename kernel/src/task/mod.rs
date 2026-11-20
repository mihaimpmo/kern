//! Tasks and the preemptive round-robin scheduler, shared by all CPUs.
//!
//! A task is one thread: a kernel thread, or a thread of a user process.
//! Every task has its own kernel stack.
//! Switching tasks = saving callee-saved registers on the old kernel stack,
//! swapping stack pointers, and popping them from the new one. Each CPU has
//! an idle task (pid 0, not in the task table) that runs when nothing else
//! is ready.
//!
//! Each CPU has its own ready queue. A task that becomes ready goes back to
//! the CPU it last ran on, whose caches may still hold its data, unless that
//! CPU is busy and another one is idle; an idle CPU is woken with an IPI so
//! it doesn't wait for its next timer tick. A CPU with nothing queued takes
//! work from the longest other queue. (The queues are still all behind the
//! one scheduler lock.)
//!
//! Locking: all scheduler state lives in `SCHED`, and the lock is held
//! across `switch_context`. The task switching away takes it; the task that
//! resumes releases it, by dropping the guard it held when it last switched
//! away, or in `task_start` if it has never run. So "mark the old task
//! ready/blocked" and "stop running on its stack" are a single step as far as
//! other CPUs can tell: none of them can pick a task up while its registers
//! are still being saved.
//!
//! Threads: a process starts with one task, and `spawn_thread` adds more
//! that share its `Process` (memory, files, working directory). The process
//! is known by the pid of its first task, the *leader*, which every thread
//! carries as `tgid`; `getpid` returns it and the parent waits for it. The
//! process ends when its last thread does. Ending the process (`exit`, a
//! fatal signal) ends every thread: `exit_current` records the exit status
//! in the `Process`, marks the others `doomed` and sends them SIGKILL, and
//! each ends itself on its way back to user mode. The leader's entry stays
//! (a zombie, once its thread is gone) until the parent collects the
//! process's status, which `wait` only does once every thread has ended.
//! Signal actions belong to the process: each thread has a copy, and
//! changes are made to all of them. A signal sent to the process goes to
//! one thread that doesn't block it.

mod elf;
pub mod exec;
pub mod signal;

use crate::arch::fpu::FpuState;
use crate::arch::interrupts::VECTOR_RESCHEDULE;
use crate::arch::{apic, cpu, percpu, smp, tlb};
use crate::fs::FdTable;
use crate::mm::uspace::{FaultResult, UserSpace};
use crate::mm::{kstack::KernelStack, vmm};
use crate::sync::{SpinLock, SpinLockGuard};
use alloc::{boxed::Box, collections::BTreeMap, collections::VecDeque, string::String, sync::Arc, vec::Vec};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub type Pid = u64;

const TIME_SLICE_MS: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Sleeping,
    Zombie,
}

/// How a process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitStatus {
    Exited(i32),
    /// Killed by this signal (sent with `kill`, or raised by a CPU fault).
    Killed(u8),
}

impl ExitStatus {
    /// As seen by user space: the exit code in the low 32 bits, or
    /// 1 << 32 | signal for a killed process.
    pub fn encode(self) -> u64 {
        match self {
            ExitStatus::Exited(code) => code as u32 as u64,
            ExitStatus::Killed(signal) => 1 << 32 | signal as u64,
        }
    }

    /// Never 0, so 0 can mean "none".
    fn to_raw(self) -> u64 {
        1 << 63 | self.encode()
    }

    fn from_raw(raw: u64) -> Option<ExitStatus> {
        match raw {
            0 => None,
            r if r >> 32 & 1 != 0 => Some(ExitStatus::Killed(r as u8)),
            r => Some(ExitStatus::Exited(r as u32 as i32)),
        }
    }
}

pub struct Task {
    /// Unique to this thread (Linux's tid).
    pub pid: Pid,
    /// The process: the leader's pid (Linux's tgid). Its own pid for a
    /// kernel thread or a process's first thread.
    pub tgid: Pid,
    pub name: String,
    pub state: State,
    /// The process that started this one (a tgid); only on leaders.
    pub parent: Option<Pid>,
    /// CPU the task is running on, while `state == Running`.
    pub cpu: Option<usize>,
    /// CPU it ran on last (or was started from): where it queues up.
    last_cpu: usize,
    exit: ExitStatus,
    /// Pending and blocked signals, and their actions.
    signals: signal::Signals,
    rsp: u64,
    kstack: Option<KernelStack>,
    /// What the user process owns (None for kernel threads). Locked
    /// separately from the scheduler.
    process: Option<Arc<Process>>,
    /// Page tables to load for this task: the space's, or the kernel's.
    pml4: u64,
    /// Saved x87/SSE registers, for user tasks (the kernel doesn't use them).
    fpu: Option<Box<FpuState>>,
    wake_at: u64,
    /// Where it enters user mode for the first time, and with what in rdi.
    user_entry: u64,
    user_stack: u64,
    user_arg: u64,
    /// FS base: the thread's thread-local storage.
    fs_base: u64,
    /// A user u32 to zero, and wake futex waiters on, when the thread ends:
    /// how `join` knows the thread no longer uses its stack (0: none).
    exit_word: u64,
    /// The process is ending (or `exec` is replacing the program): this
    /// thread must end too, whatever signal tells it.
    doomed: bool,
    /// In `exit_thread`, past the point of counting as a live thread.
    exiting: bool,
    /// On a leader: its process has ended (`end_process` has run), though
    /// some of its threads may not be zombies yet.
    ended: bool,
}

impl Task {
    fn new(pid: Pid, name: &str, parent: Option<Pid>) -> Task {
        Task {
            pid,
            tgid: pid,
            name: String::from(name),
            state: State::Ready,
            parent,
            cpu: None,
            last_cpu: 0,
            exit: ExitStatus::Exited(0),
            signals: signal::Signals::new(),
            rsp: 0,
            kstack: None,
            process: None,
            pml4: vmm::kernel_pml4(),
            fpu: None,
            wake_at: 0,
            user_entry: 0,
            user_stack: 0,
            user_arg: 0,
            fs_base: 0,
            exit_word: 0,
            doomed: false,
            exiting: false,
            ended: false,
        }
    }

    fn alive(&self) -> bool {
        self.state != State::Zombie && !self.exiting
    }

    /// A process whose exit status can be collected: the leader, once every
    /// thread has ended.
    fn collectable(&self) -> bool {
        self.state == State::Zombie && self.ended
    }
}

/// A user process's resources, as opposed to its scheduling state.
pub struct Process {
    pub space: SpinLock<UserSpace>,
    pub files: SpinLock<FdTable>,
    /// Canonical absolute path of the working directory.
    pub cwd: SpinLock<String>,
    /// Process group, for signalling a whole job (Ctrl+C).
    pub pgid: AtomicU64,
    /// How the process ends, once something has ended it as a whole
    /// (`ExitStatus::to_raw`; 0 while it runs).
    exit: AtomicU64,
}

impl Process {
    pub fn new(space: UserSpace, inherit: Inherit) -> Process {
        Process {
            space: SpinLock::new(space),
            files: SpinLock::new(inherit.files),
            cwd: SpinLock::new(inherit.cwd),
            pgid: AtomicU64::new(inherit.pgid),
            exit: AtomicU64::new(0),
        }
    }

    fn exit_status(&self) -> Option<ExitStatus> {
        ExitStatus::from_raw(self.exit.load(Ordering::Relaxed))
    }
}

/// What a new process starts with besides its program.
pub struct Inherit {
    pub files: FdTable,
    pub cwd: String,
    pub pgid: Pid,
}

/// What the scheduler knows about one CPU.
struct Cpu {
    /// The task running there: in `tasks`, or this CPU's `idle`. Both are
    /// boxed, so the pointer stays valid while the task exists, and a task
    /// is only removed from `tasks` once it can't be current anywhere.
    current: *mut Task,
    idle: Box<Task>,
    slice: u32,
    /// Tasks waiting to run here.
    ready: VecDeque<Pid>,
    /// An IPI is on its way here and the CPU hasn't scheduled since.
    kicked: bool,
}

impl Cpu {
    fn is_idle(&self) -> bool {
        core::ptr::eq(self.current, &*self.idle)
    }
}

struct Scheduler {
    tasks: BTreeMap<Pid, Box<Task>>,
    sleeping: Vec<Pid>,
    zombies: Vec<Pid>, // exited, resources not yet freed
    cpus: Vec<Cpu>,    // indexed by percpu::index()
    next_pid: Pid,
}

// The raw `current` pointers are only used with the lock held.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    tasks: BTreeMap::new(),
    sleeping: Vec::new(),
    zombies: Vec::new(),
    cpus: Vec::new(),
    next_pid: 1,
});
static TICKS: AtomicU64 = AtomicU64::new(0);
static CHILD_EXITED: WaitQueue = WaitQueue::new();
static THREAD_EXITED: WaitQueue = WaitQueue::new();

type SchedGuard = SpinLockGuard<'static, Scheduler>;

impl Scheduler {
    fn this_cpu(&mut self) -> &mut Cpu {
        let index = percpu::index();
        &mut self.cpus[index]
    }

    fn current(&mut self) -> &mut Task {
        unsafe { &mut *self.this_cpu().current }
    }

    fn make_ready(&mut self, pid: Pid) {
        let Some(t) = self.tasks.get_mut(&pid) else { return };
        t.state = State::Ready;
        let last = t.last_cpu;
        // Stay put unless the CPU is busy and another one has nothing to do.
        let target = if self.cpus[last].is_idle() && self.cpus[last].ready.is_empty() {
            last
        } else {
            (0..self.cpus.len()).find(|&c| self.cpus[c].is_idle() && self.cpus[c].ready.is_empty()).unwrap_or(last)
        };
        self.cpus[target].ready.push_back(pid);
        if self.cpus[target].is_idle() {
            self.kick(target);
        }
    }

    /// Interrupt CPU `index` (unless it's this one, or already on its way)
    /// so it reschedules, or notices a signal, now.
    fn kick(&mut self, index: usize) {
        if index == percpu::index() || self.cpus[index].kicked {
            return;
        }
        self.cpus[index].kicked = true;
        apic::send_ipi(smp::lapic_id_of(index), VECTOR_RESCHEDULE as u8);
    }

    /// The live threads of process `tgid`.
    fn threads(&self, tgid: Pid) -> impl Iterator<Item = &Task> {
        self.tasks.values().map(|t| &**t).filter(move |t| t.tgid == tgid && t.alive())
    }

    /// Whether any CPU has a task waiting.
    fn any_ready(&self) -> bool {
        self.cpus.iter().any(|c| !c.ready.is_empty())
    }

    /// The next task for CPU `index`: from its own queue, or else the front
    /// of the longest other queue.
    fn pop_ready(&mut self, index: usize) -> Option<Pid> {
        if let Some(pid) = self.cpus[index].ready.pop_front() {
            return Some(pid);
        }
        let busiest = (0..self.cpus.len()).max_by_key(|&c| self.cpus[c].ready.len())?;
        self.cpus[busiest].ready.pop_front()
    }
}

// ------------------------------------------------------------ context switch

extern "C" {
    fn switch_context(old_rsp: *mut u64, new_rsp: u64);
    fn task_trampoline();
    fn enter_user(rip: u64, rsp: u64, arg: u64) -> !;
}

global_asm!(
    r#"
.section .text
.global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

// First code a new task runs: r12 = entry function, r13 = argument.
.global task_trampoline
task_trampoline:
    and rsp, -16
    mov rdi, r13
    mov rsi, r12
    call task_start
    ud2

// Drop to ring 3 at rip = rdi with stack rsp = rsi and rdi = rdx.
.global enter_user
enter_user:
    cli
    swapgs
    push 0x1b          // SS  = user data
    push rsi           // RSP
    push 0x202         // RFLAGS: interrupts on
    push 0x23          // CS  = user code
    push rdi           // RIP
    mov rdi, rdx
    xor eax, eax
    xor ebx, ebx
    xor ecx, ecx
    xor edx, edx
    xor esi, esi
    xor ebp, ebp
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor r11d, r11d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    iretq
"#
);

#[no_mangle]
extern "C" fn task_start(arg: u64, entry: u64) -> ! {
    // We were switched to with the scheduler lock held, and have no guard
    // of our own to drop (see the module comment).
    unsafe { SCHED.force_unlock() };
    cpu::sti();
    let f: fn(u64) = unsafe { core::mem::transmute(entry as usize) };
    f(arg);
    exit_current(ExitStatus::Exited(0));
}

/// Lay out a fresh kernel stack so that `switch_context` "returns" into
/// `task_trampoline` with the entry point and argument in r12/r13.
fn prepare_stack(top: u64, entry: fn(u64), arg: u64) -> u64 {
    let rsp = top - 8 * 8;
    let frame = rsp as *mut u64;
    unsafe {
        frame.add(0).write(0); // r15
        frame.add(1).write(0); // r14
        frame.add(2).write(arg); // r13
        frame.add(3).write(entry as usize as u64); // r12
        frame.add(4).write(0); // rbx
        frame.add(5).write(0); // rbp
        frame.add(6).write(task_trampoline as *const () as u64); // return address
        frame.add(7).write(0);
    }
    rsp
}

// ---------------------------------------------------------------- lifecycle

/// Register the calling CPU with the scheduler; its boot thread becomes its
/// idle task. CPUs must call this in index order (the boot CPU first).
pub fn add_cpu() {
    let mut s = SCHED.lock();
    assert_eq!(s.cpus.len(), percpu::index(), "CPUs registered out of order");
    let mut idle = Box::new(Task::new(0, "idle", None)); // runs on the stack it was started on
    idle.state = State::Running;
    idle.cpu = Some(percpu::index());
    let current: *mut Task = &mut *idle;
    s.cpus.push(Cpu { current, idle, slice: TIME_SLICE_MS, ready: VecDeque::new(), kicked: false });
}

/// A task that will run `entry(arg)` on a fresh kernel stack; `insert`
/// gives it a pid.
fn new_task(name: &str, parent: Option<Pid>, entry: fn(u64), arg: u64) -> Option<Box<Task>> {
    let kstack = KernelStack::new()?;
    let mut task = Box::new(Task::new(0, name, parent));
    task.rsp = prepare_stack(kstack.top(), entry, arg);
    task.kstack = Some(kstack);
    task.last_cpu = percpu::index(); // start near the parent
    Some(task)
}

/// A user task of `process`, entering user mode at `rip` with `rsp`.
fn new_user_task(name: &str, parent: Option<Pid>, process: Arc<Process>, rip: u64, rsp: u64) -> Option<Box<Task>> {
    let mut task = new_task(name, parent, user_task_entry, 0)?;
    task.pml4 = process.space.lock().pml4();
    task.process = Some(process);
    task.fpu = Some(FpuState::new());
    task.user_entry = rip;
    task.user_stack = rsp;
    Some(task)
}

/// Add `task` to the table, ready to run; returns its pid. Unless it was
/// given a process to join (`tgid`), it starts its own.
fn insert(s: &mut Scheduler, mut task: Box<Task>) -> Pid {
    let pid = s.next_pid;
    s.next_pid += 1;
    task.pid = pid;
    if task.tgid == 0 {
        task.tgid = pid;
    }
    s.tasks.insert(pid, task);
    s.make_ready(pid);
    pid
}

/// Start a kernel thread running `entry(arg)`.
#[allow(dead_code)]
pub fn spawn_kernel(name: &str, entry: fn(u64), arg: u64) -> Option<Pid> {
    let task = new_task(name, None, entry, arg)?;
    Some(insert(&mut SCHED.lock(), task))
}

/// Load an ELF executable into a new address space and start it in ring 3
/// with the given arguments (`args[0]` is conventionally the program name).
fn spawn_user(
    name: &str,
    image: &[u8],
    args: &[&[u8]],
    parent: Option<Pid>,
    inherit: Inherit,
) -> Result<Pid, exec::LoadError> {
    let image = exec::load(image, args)?;
    let process = Arc::new(Process::new(image.space, inherit));
    let task = new_user_task(name, parent, process, image.entry, image.stack).ok_or(exec::LoadError::NoMemory)?;
    Ok(insert(&mut SCHED.lock(), task))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadError {
    NoMemory,
    /// The process is ending.
    Exiting,
}

/// Start another thread in the calling process: it enters user mode at
/// `rip` with stack `rsp`, `arg` in rdi and FS base `tls`. It has the
/// caller's signal mask and none of its pending signals. When it ends, the
/// u32 at `exit_word` (if not 0) is zeroed and its futex waiters woken.
pub fn spawn_thread(rip: u64, rsp: u64, arg: u64, tls: u64, exit_word: u64) -> Result<Pid, ThreadError> {
    let me = current_process().expect("thread from a kernel thread");
    let name = SCHED.lock().current().name.clone();
    let mut task = new_user_task(&name, None, me, rip, rsp).ok_or(ThreadError::NoMemory)?;
    task.user_arg = arg;
    task.fs_base = tls;
    task.exit_word = exit_word;
    let mut s = SCHED.lock();
    let creator = s.current();
    // Checked under the lock that `exit_current` dooms threads under, so
    // the new thread can't miss the end of its process.
    let ending = creator.process.as_ref().map_or(true, |p| p.exit_status().is_some());
    if creator.doomed || ending {
        drop(s);
        return Err(ThreadError::Exiting);
    }
    task.tgid = creator.tgid;
    task.signals = creator.signals.for_new_thread();
    Ok(insert(&mut s, task))
}

#[derive(Clone, Copy, Debug)]
pub enum SpawnError {
    Fs(crate::fs::FsError),
    Load(exec::LoadError),
    /// `exec`: the process was ended meanwhile.
    Interrupted,
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            SpawnError::Fs(e) => write!(f, "{:?}", e),
            SpawnError::Load(e) => write!(f, "{}", e),
            SpawnError::Interrupted => write!(f, "interrupted"),
        }
    }
}

/// Start the program at `path`, resolved against `inherit.cwd`. A bare name
/// (no '/') means /bin/<name>. The task is named after the last component.
pub fn spawn_program(path: &str, args: &[&[u8]], parent: Option<Pid>, inherit: Inherit) -> Result<Pid, SpawnError> {
    let (name, image) = read_program(&inherit.cwd, path)?;
    spawn_user(&name, &image, args, parent, inherit).map_err(SpawnError::Load)
}

/// The executable at `path` (a bare name means /bin/<name>), and the name
/// its task goes by: the last path component.
fn read_program(cwd: &str, path: &str) -> Result<(String, Vec<u8>), SpawnError> {
    let full = if path.contains('/') { String::from(path) } else { alloc::format!("/bin/{}", path) };
    let inode = crate::fs::lookup(cwd, &full).map_err(SpawnError::Fs)?;
    let image = crate::fs::read_file(&inode).map_err(SpawnError::Fs)?;
    let name = String::from(full.rsplit('/').next().unwrap_or(&full));
    Ok((name, image))
}

/// Replace the calling process's program with the one at `path`. The
/// process keeps its pid, parent, process group, working directory and
/// file descriptors, except those marked close-on-spawn (O_CLOEXEC), which
/// are closed. Handlers go back to the default action. Returns the entry
/// point and stack pointer to return to user mode with; on an error the
/// old program carries on unchanged.
pub fn exec_current(path: &str, args: &[&[u8]]) -> Result<(u64, u64), SpawnError> {
    let process = current_process().expect("exec from a kernel thread");
    let cwd = process.cwd.lock().clone();
    let (name, data) = read_program(&cwd, path)?;
    let image = exec::load(&data, args).map_err(SpawnError::Load)?;
    drop(data);
    let fpu = FpuState::new();
    // The new program starts with this thread alone.
    if end_other_threads().is_err() {
        return Err(SpawnError::Interrupted);
    }

    // The point of no return. Until CR3 is switched below, the task keeps
    // running on the old page tables, which stay alive in `old`.
    let pml4 = image.space.pml4();
    let old = core::mem::replace(&mut *process.space.lock(), image.space);
    let closing = process.files.lock().take_close_on_spawn();
    {
        let mut s = SCHED.lock();
        let t = s.current();
        t.pml4 = pml4;
        tlb::switch_to(pml4);
        t.name = name;
        t.signals.reset_for_exec();
        t.fs_base = 0;
        unsafe { cpu::wrmsr(cpu::msr::FS_BASE, 0) };
        t.exit_word = 0;
        // The live registers are the task's (the kernel never uses them).
        fpu.restore();
        t.fpu = Some(fpu);
    }
    drop(process);
    drop(old); // frees the old program's memory, now that nothing uses it
    drop(closing); // closing can wake other tasks: no locks held
    Ok((image.entry, image.stack))
}

fn user_task_entry(_: u64) {
    let (rip, rsp, arg) = {
        let mut s = SCHED.lock();
        let t = s.current();
        (t.user_entry, t.user_stack, t.user_arg)
    };
    unsafe { enter_user(rip, rsp, arg) }; // `schedule` has loaded FS base
}

/// End every other thread of the calling process and wait until they have
/// (for `exec`). Fails if this thread is to end as well.
fn end_other_threads() -> Result<(), Interrupted> {
    let mut s = SCHED.lock();
    loop {
        let me = s.current();
        if me.doomed || me.signals.deliverable() & signal::bit(signal::SIGKILL) != 0 {
            return Err(Interrupted);
        }
        let (pid, tgid) = (me.pid, me.tgid);
        let others: Vec<Pid> =
            s.tasks.values().filter(|t| t.tgid == tgid && t.pid != pid && t.state != State::Zombie).map(|t| t.pid).collect();
        if others.is_empty() {
            return Ok(());
        }
        for other in others {
            doom(&mut s, other);
        }
        THREAD_EXITED.block_locked(&mut s);
    }
}

/// Make thread `pid` end on its way back to user mode, waking it if needed.
fn doom(s: &mut Scheduler, pid: Pid) {
    if let Some(t) = s.tasks.get_mut(&pid) {
        if !t.doomed {
            t.doomed = true;
            post_locked(s, pid, signal::SIGKILL);
        }
    }
}

/// End the calling process, every thread of it, with `status` (unless it
/// is already ending, with a status of its own).
pub fn exit_current(status: ExitStatus) -> ! {
    {
        let mut s = SCHED.lock();
        let me = s.current();
        let (pid, tgid) = (me.pid, me.tgid);
        let first = me.process.as_ref().map_or(false, |p| {
            p.exit.compare_exchange(0, status.to_raw(), Ordering::Relaxed, Ordering::Relaxed).is_ok()
        });
        if first {
            let others: Vec<Pid> = s.threads(tgid).filter(|t| t.pid != pid).map(|t| t.pid).collect();
            for other in others {
                doom(&mut s, other);
            }
        }
    }
    exit_thread(status)
}

/// End the calling thread. Its memory is freed by the scheduler once we
/// have switched away from its stack. If it was the last thread of its
/// process, the process ends: with the status it was ended with, if any,
/// else with `status`.
pub fn exit_thread(status: ExitStatus) -> ! {
    let process = current_process();
    let (pid, tgid, exit_word, last) = {
        let mut s = SCHED.lock();
        let me = s.current();
        assert!(me.pid != 0, "idle task tried to exit");
        me.exiting = true;
        let (pid, tgid, exit_word) = (me.pid, me.tgid, me.exit_word);
        let last = s.threads(tgid).next().is_none();
        (pid, tgid, exit_word, last)
    };
    let status = process.as_ref().and_then(|p| p.exit_status()).filter(|_| last).unwrap_or(status);
    if exit_word != 0 {
        // A thread that `join`s us may free our stack once it sees this.
        let _ = crate::syscall::put_user_u32(exit_word, 0);
    }
    // Close the process's files now, outside the scheduler lock: closing
    // can wake other tasks (the reader at the other end of a pipe, say).
    if let (true, Some(process)) = (last, &process) {
        let files = process.files.lock().take_all();
        drop(files);
    }
    drop(process);

    let mut s = SCHED.lock();
    if last && tgid == 1 {
        drop(s);
        crate::power::init_exited(status);
    }
    if last {
        end_process(&mut s, tgid, status);
    }
    let t = s.current();
    t.state = State::Zombie;
    t.exit = status;
    s.zombies.push(pid);
    THREAD_EXITED.wake_all_locked(&mut s);
    schedule(&mut s);
    unreachable!("zombie task was scheduled");
}

/// The last thread of process `tgid` is ending: give the leader's entry the
/// process's exit status, orphan its children and tell its parent.
fn end_process(s: &mut SchedGuard, tgid: Pid, status: ExitStatus) {
    // Orphan our children. A child still running, or exited but not yet
    // reaped, is removed by `reap` once it has no parent. A child that was
    // already reaped is only waiting for us to collect its exit code, which
    // will now never happen, so drop its entry here.
    let s_ref = &mut **s;
    let zombies = &s_ref.zombies;
    s_ref.tasks.retain(|_, t| {
        if t.parent != Some(tgid) {
            return true;
        }
        if t.state == State::Zombie && !zombies.contains(&t.pid) {
            return false;
        }
        t.parent = None;
        true
    });
    let Some(leader) = s.tasks.get_mut(&tgid) else { return };
    leader.exit = status;
    leader.ended = true;
    let (parent, reaped) = (leader.parent, leader.state == State::Zombie && !s.zombies.contains(&tgid));
    match parent {
        Some(parent) => {
            post_to_process(s, parent, signal::SIGCHLD);
            CHILD_EXITED.wake_all_locked(s);
        }
        // Nobody will collect it. A leader already reaped (its thread ended
        // before ours) is only waiting for that; otherwise `reap` drops it.
        None if reaped => {
            s.tasks.remove(&tgid);
        }
        None => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitError {
    NotChild,
    /// A signal arrived while waiting.
    Interrupted,
}

/// Wait for child `pid` to end and return how it did.
pub fn wait(pid: Pid) -> Result<ExitStatus, WaitError> {
    let mut s = SCHED.lock();
    loop {
        let me = s.current().tgid;
        let t = s.tasks.get(&pid).ok_or(WaitError::NotChild)?;
        if t.parent != Some(me) {
            return Err(WaitError::NotChild);
        }
        if t.collectable() {
            let status = t.exit;
            collect(&mut s, pid);
            return Ok(status);
        }
        if s.current().signals.deliverable() != 0 {
            return Err(WaitError::Interrupted);
        }
        CHILD_EXITED.block_locked(&mut s);
    }
}

/// Collect any one ended child without blocking: Some((pid, status)).
pub fn try_wait_any() -> Option<(Pid, ExitStatus)> {
    let mut s = SCHED.lock();
    let me = s.current().tgid;
    let (pid, status) = s.tasks.values().find(|t| t.parent == Some(me) && t.collectable()).map(|t| (t.pid, t.exit))?;
    collect(&mut s, pid);
    Some((pid, status))
}

// ---------------------------------------------------------------- signals

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillError {
    /// No such process (or group), or only zombies.
    NotFound,
    /// Pid 1 can't be signalled: the system would stop.
    Init,
}

#[derive(Clone, Copy)]
pub enum KillTarget {
    Process(Pid),
    Group(Pid),
}

/// Send `signal` to a process or a process group. The target acts on it the
/// next time it would return to user mode (see `signal`); a sleeping or
/// blocked target is woken so that happens soon, unless it blocks or ignores
/// the signal.
pub fn kill(target: KillTarget, sig: u8) -> Result<(), KillError> {
    let mut s = SCHED.lock();
    let matches = |t: &Task| match target {
        KillTarget::Process(pid) => t.tgid == pid,
        KillTarget::Group(pgid) => t.process.as_ref().map_or(false, |p| p.pgid.load(Ordering::Relaxed) == pgid),
    };
    let mut processes: Vec<Pid> = s.tasks.values().filter(|t| t.alive() && matches(t)).map(|t| t.tgid).collect();
    processes.sort_unstable();
    processes.dedup();
    if let KillTarget::Process(1) = target {
        return Err(KillError::Init);
    }
    let mut sent = false;
    for tgid in processes.into_iter().filter(|&p| p != 1) {
        sent |= post_to_process(&mut s, tgid, sig);
    }
    if sent { Ok(()) } else { Err(KillError::NotFound) }
}

/// Post `sig` to process `tgid`: to one thread that can take it now, or
/// else the leader (or any thread) to hold it pending. False if the process
/// has no live thread.
fn post_to_process(s: &mut Scheduler, tgid: Pid, sig: u8) -> bool {
    let threads = || s.threads(tgid);
    let takes = |t: &&Task| t.signals.blocked() & signal::bit(sig) == 0;
    let to = threads().find(takes).or_else(|| threads().find(|t| t.pid == tgid)).or_else(|| threads().next());
    match to.map(|t| t.pid) {
        Some(pid) => {
            post_locked(s, pid, sig);
            true
        }
        None => false,
    }
}

/// Post `sig` to task `pid` (if it is a live user task), waking it if it
/// sleeps or blocks and can act on the signal now.
fn post_locked(s: &mut Scheduler, pid: Pid, sig: u8) {
    let Some(t) = s.tasks.get_mut(&pid) else { return };
    if t.state == State::Zombie || t.process.is_none() {
        return;
    }
    if !t.signals.post(sig) {
        return;
    }
    match (t.state, t.cpu) {
        (State::Blocked | State::Sleeping, _) => s.make_ready(pid),
        // Running elsewhere: interrupt it, so it acts on the signal on its
        // way back to user mode instead of at its next timer tick.
        (State::Running, Some(cpu)) => s.kick(cpu),
        _ => {}
    }
}

/// Put process `pid` in group `pgid`. Only the caller and its children can
/// be moved.
pub fn set_process_group(pid: Pid, pgid: Pid) -> Result<(), KillError> {
    let mut s = SCHED.lock();
    let me = s.current().tgid;
    let t = s.tasks.get(&pid).filter(|t| t.state != State::Zombie).ok_or(KillError::NotFound)?;
    if pid != me && t.parent != Some(me) {
        return Err(KillError::NotFound);
    }
    t.process.as_ref().ok_or(KillError::NotFound)?.pgid.store(pgid, Ordering::Relaxed);
    Ok(())
}

/// Run `f` on the calling task's signal state, under the scheduler lock.
/// `f` must not take other locks or print.
pub fn with_signals<R>(f: impl FnOnce(&mut signal::Signals) -> R) -> R {
    f(&mut SCHED.lock().current().signals)
}

/// Set (if `new`) the action for `sig` in every thread of the calling
/// process; returns the old one.
pub fn signal_action(sig: u8, new: Option<signal::Action>) -> signal::Action {
    let mut s = SCHED.lock();
    let old = s.current().signals.get_action(sig);
    if let Some(action) = new {
        let tgid = s.current().tgid;
        for t in s.tasks.values_mut().filter(|t| t.tgid == tgid && t.alive()) {
            t.signals.set_action(sig, action);
        }
    }
    old
}

/// On the way back to user mode: the next signal to act on, if any. A
/// signal whose action is to end the process does so here.
pub fn next_signal() -> Option<(u8, signal::Action, u64)> {
    let (next, doomed) = {
        let mut s = SCHED.lock();
        let me = s.current();
        let (next, doomed, tgid) = (me.signals.next(), me.doomed, me.tgid);
        if let Some(signal::Disposition::Handle { sig, action, .. }) = &next {
            if action.flags & signal::SA_RESETHAND != 0 {
                for t in s.tasks.values_mut().filter(|t| t.tgid == tgid) {
                    t.signals.set_action(*sig, signal::Action::default());
                }
            }
        }
        (next, doomed)
    };
    match next? {
        // Another thread is ending the process, or replacing its program.
        signal::Disposition::Terminate(sig) if doomed => exit_thread(ExitStatus::Killed(sig)),
        signal::Disposition::Terminate(sig) => exit_current(ExitStatus::Killed(sig)),
        signal::Disposition::Handle { sig, action, restore_mask } => Some((sig, action, restore_mask)),
    }
}

/// A CPU fault in user mode raised `sig`. If the process has a handler for
/// it that isn't blocked, post it (the handler runs on the way back to user
/// mode) and return true; otherwise the caller ends the process.
pub fn raise_fault(sig: u8) -> bool {
    with_signals(|s| s.fault_is_caught(sig) && {
        s.post(sig);
        true
    })
}

/// Sleep with `mask` as the signal mask until a signal arrives that can be
/// acted on. The old mask comes back once that signal's handler returns.
pub fn sigsuspend(mask: u64) {
    with_signals(|s| {
        s.saved_mask = Some(s.blocked());
        s.set_blocked(mask);
    });
    while with_signals(|s| s.deliverable()) == 0 {
        sleep_ms(u64::MAX); // returns early for a signal
    }
}


/// The parent has the exit code of zombie `pid`: forget the task, or let the
/// reaper do so if it still owns resources.
fn collect(s: &mut Scheduler, pid: Pid) {
    if s.zombies.contains(&pid) {
        s.tasks.get_mut(&pid).unwrap().parent = None; // reap removes it
    } else {
        s.tasks.remove(&pid);
    }
}

/// Free kernel stacks and address spaces of exited tasks. Any zombie except
/// this CPU's current task is safe: with the lock held, no other CPU can be
/// running on a zombie's stack (a zombie releases the lock only by switching
/// away from it).
fn reap(s: &mut Scheduler) {
    let current = s.this_cpu().current;
    let mut i = 0;
    while i < s.zombies.len() {
        let pid = s.zombies[i];
        let t = s.tasks.get_mut(&pid).unwrap();
        if core::ptr::eq(&**t, current) {
            i += 1;
            continue;
        }
        s.zombies.swap_remove(i);
        t.kstack = None; // back to the stack pool
        t.process = None; // the last reference: frees every page the process owned
        // A thread other than the leader, or a process nobody will wait for
        // (whose parent has ended; see `end_process`).
        if t.parent.is_none() && (t.pid != t.tgid || t.ended) {
            s.tasks.remove(&pid);
        }
    }
}

// ---------------------------------------------------------------- scheduling

/// Pick the next task for this CPU and switch to it. Returns when the
/// calling task is scheduled again, with the lock held again.
fn schedule(s: &mut SchedGuard) {
    reap(s);
    let index = percpu::index();
    let prev_ptr = s.cpus[index].current;
    let prev = unsafe { &mut *prev_ptr };
    let prev_running = prev.state == State::Running;
    let prev_idle = core::ptr::eq(prev_ptr, &*s.cpus[index].idle);

    s.cpus[index].kicked = false;
    let next_ptr: *mut Task = loop {
        match s.pop_ready(index) {
            Some(pid) => match s.tasks.get_mut(&pid) {
                Some(t) if t.state == State::Ready => break &mut **t,
                _ => {} // stale entry: the task exited or was woken twice
            },
            None if prev_running && !prev_idle => break prev_ptr,
            None => break &mut *s.cpus[index].idle,
        }
    };
    s.cpus[index].slice = TIME_SLICE_MS;
    if next_ptr == prev_ptr {
        prev.state = State::Running;
        return;
    }

    if prev_running {
        prev.state = State::Ready;
        if !prev_idle {
            s.cpus[index].ready.push_back(prev.pid);
        }
    }
    prev.cpu = None;

    let next = unsafe { &mut *next_ptr };
    next.state = State::Running;
    next.cpu = Some(index);
    next.last_cpu = index;
    if let Some(stack) = &next.kstack {
        percpu::set_kernel_stack(stack.top());
    }
    if cpu::read_cr3() & !0xfff != next.pml4 {
        tlb::switch_to(next.pml4);
    }
    // The kernel never uses these registers, so swapping them here, before
    // the stack switch, is the same as doing it on the way to user mode.
    if let Some(fpu) = prev.fpu.as_mut() {
        fpu.save();
    }
    if let Some(fpu) = next.fpu.as_ref() {
        fpu.restore();
        // A user task's thread-local storage (kernel code doesn't use FS).
        unsafe { cpu::wrmsr(cpu::msr::FS_BASE, next.fs_base) };
    }
    s.cpus[index].current = next_ptr;
    let (prev_rsp, next_rsp) = (&mut prev.rsp as *mut u64, next.rsp);
    unsafe { switch_context(prev_rsp, next_rsp) };
}

/// Called from the timer interrupt on every CPU, 1000 times per second.
pub fn timer_tick() {
    let mut s = SCHED.lock();
    if percpu::index() == 0 {
        let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        wake_sleepers(&mut s, now);
    }
    let cpu = s.this_cpu();
    cpu.slice = cpu.slice.saturating_sub(1);
    let want_switch = cpu.slice == 0 || cpu.is_idle();
    if want_switch && s.any_ready() {
        schedule(&mut s);
    }
}

/// The reschedule IPI: another CPU queued work for us while we were idle
/// (or sent a signal to the task running here, which the interrupt's
/// return path takes care of).
pub fn reschedule_ipi() {
    let mut s = SCHED.lock();
    s.this_cpu().kicked = false;
    if s.this_cpu().is_idle() && s.any_ready() {
        schedule(&mut s);
    }
}

fn wake_sleepers(s: &mut Scheduler, now: u64) {
    let mut i = 0;
    while i < s.sleeping.len() {
        let pid = s.sleeping[i];
        match s.tasks.get(&pid) {
            Some(t) if t.state == State::Sleeping && t.wake_at > now => i += 1,
            Some(t) if t.state == State::Sleeping => {
                s.sleeping.swap_remove(i);
                s.make_ready(pid);
            }
            _ => {
                s.sleeping.swap_remove(i);
            }
        }
    }
}

pub fn yield_now() {
    schedule(&mut SCHED.lock());
}

/// Sleep for `ms` milliseconds, or until a signal arrives.
pub fn sleep_ms(ms: u64) {
    let mut s = SCHED.lock();
    let t = s.current();
    if t.signals.deliverable() != 0 {
        return;
    }
    t.wake_at = uptime_ms().saturating_add(ms.max(1));
    t.state = State::Sleeping;
    let pid = t.pid;
    s.sleeping.push(pid);
    schedule(&mut s);
}

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// The calling process: its leader's pid (`getpid`).
pub fn current_pid() -> Pid {
    SCHED.lock().current().tgid
}

/// The calling thread (`gettid`).
pub fn current_tid() -> Pid {
    SCHED.lock().current().pid
}

/// Set the calling thread's FS base (its thread-local storage pointer).
pub fn set_fs_base(base: u64) {
    let mut s = SCHED.lock();
    s.current().fs_base = base;
    unsafe { cpu::wrmsr(cpu::msr::FS_BASE, base) };
}

/// The running user process; None for kernel threads. Drop the result
/// before exiting: a reference left on a dead task's stack is never dropped.
pub fn current_process() -> Option<Arc<Process>> {
    SCHED.lock().current().process.clone()
}

/// A page fault from user mode on a page that isn't mapped.
pub fn user_page_fault(addr: u64, write: bool, exec: bool) -> FaultResult {
    match current_process() {
        Some(process) => process.space.lock().handle_fault(addr, write, exec),
        None => FaultResult::Denied,
    }
}

pub struct TaskInfo {
    pub pid: Pid,
    pub tgid: Pid,
    pub parent: Option<Pid>,
    pub state: State,
    pub cpu: Option<usize>,
    pub name: String,
}

/// Every task except the idle tasks.
pub fn list() -> Vec<TaskInfo> {
    let s = SCHED.lock();
    s.tasks
        .values()
        .map(|t| TaskInfo { pid: t.pid, tgid: t.tgid, parent: t.parent, state: t.state, cpu: t.cpu, name: t.name.clone() })
        .collect()
}

/// The idle loop: runs whenever nothing else is ready on this CPU.
pub fn idle() -> ! {
    loop {
        crate::mm::kstack::trim();
        cpu::sti_hlt();
    }
}

// -------------------------------------------------------------- wait queues

/// A signal interrupted a wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interrupted;

/// Tasks sleeping until some event happens, used like a condition variable:
/// check the condition with its lock held, then `wait` with that lock's
/// guard. A `wake_all` from another CPU can't slip in between the check and
/// going to sleep, because `wait` releases the lock only once the task is
/// already on the queue.
///
/// Lock order: the caller's lock, then the scheduler, then `waiters`.
pub struct WaitQueue {
    waiters: SpinLock<Vec<Pid>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self { waiters: SpinLock::new(Vec::new()) }
    }

    /// Like `wait`, but gives up if a signal arrives (or is already pending),
    /// returning with the lock released.
    pub fn wait_interruptible<'a, T>(&self, guard: SpinLockGuard<'a, T>) -> Result<SpinLockGuard<'a, T>, Interrupted> {
        let mut s = SCHED.lock();
        if s.current().signals.deliverable() != 0 {
            drop(s);
            return Err(Interrupted);
        }
        self.enqueue_current(&mut s);
        let (lock, irq) = guard.release();
        schedule(&mut s);
        let interrupted = s.current().signals.deliverable() != 0;
        drop(s);
        if interrupted {
            return Err(Interrupted); // `irq` restores the interrupt state
        }
        Ok(lock.lock_with(irq))
    }

    /// Release `guard`, sleep until woken, then take the lock again. Wakeups
    /// can be spurious: re-check the condition in a loop. Signals don't cut
    /// this short; use `wait_interruptible` where they should.
    pub fn wait<'a, T>(&self, guard: SpinLockGuard<'a, T>) -> SpinLockGuard<'a, T> {
        let mut s = SCHED.lock();
        // Queue up *before* releasing the caller's lock: whoever changes the
        // condition next (under that lock) then finds us in `waiters`.
        self.enqueue_current(&mut s);
        let (lock, irq) = guard.release();
        schedule(&mut s);
        drop(s);
        lock.lock_with(irq)
    }

    /// Sleep on the queue when the condition is scheduler state itself.
    fn block_locked(&self, s: &mut SchedGuard) {
        self.enqueue_current(s);
        schedule(s);
    }

    fn enqueue_current(&self, s: &mut Scheduler) {
        let t = s.current();
        t.state = State::Blocked;
        self.waiters.lock().push(t.pid);
    }

    /// Wake every task sleeping on the queue. Don't hold the scheduler lock.
    /// Call it after changing the condition and releasing its lock.
    pub fn wake_all(&self) {
        // Cheap when nobody waits: a waiter queues itself before releasing
        // the condition's lock, so it's either visible here already or will
        // see the new condition itself.
        if self.waiters.lock().is_empty() {
            return;
        }
        self.wake_all_locked(&mut SCHED.lock());
    }

    fn wake_all_locked(&self, s: &mut Scheduler) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for pid in waiters {
            if s.tasks.get(&pid).map_or(false, |t| t.state == State::Blocked) {
                s.make_ready(pid);
            }
        }
    }
}
