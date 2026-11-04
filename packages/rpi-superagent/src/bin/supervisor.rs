//! The super-agent supervisor: a process that outlives the session that
//! started it.
//!
//! A host that dies leaves its children running, and nothing inside the
//! session can clean them up — the session is what died. This program is the
//! cleaner: it sweeps the session transcripts for the children a dead host
//! left behind and kills them.
//!
//! It is a separate program rather than a thread because a thread dies with
//! its host. The plugin starts it with `exec`, so the process is clean from
//! the start and shares nothing with the host's memory.

fn main() {
    // Detach from the session that started this process. The supervisor has to
    // outlive its host: a host that dies leaves children running, and nothing
    // else will clean them up. Without this, the supervisor would be in the
    // host's process group and would be swept away with it.
    rpi_superagent::supervisor::detach();

    // Only one supervisor may run: the lock decides. A second process finds it
    // taken and leaves the work to the first.
    let Some(_lock) = rpi_superagent::supervisor::take_lock() else {
        // Another supervisor already holds the lock.
        std::process::exit(0);
    };
    // The lock lives as long as this process: it is held by the open
    // descriptor, and the kernel releases it when the process dies.
    std::mem::forget(_lock);
    rpi_superagent::supervisor::supervise();
}
