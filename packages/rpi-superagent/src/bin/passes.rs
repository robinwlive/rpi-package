//! The pass scheduler: a process that outlives the session that started it.
//!
//! ## Why a program rather than a thread
//!
//! A thread dies with its host. A wave queued in a session that then exits would
//! never run — the queue would accept the work and silently drop it, which is
//! worse than refusing. DeepSeek's prototype never has this problem because its
//! Runtime is the application; here the host is `rpi`, and it comes and goes.
//!
//! The package already solves this for orphan cleanup: `rpi-superagent-supervisor`
//! is a separate program, started with `exec`, detached with `setsid`, so it is
//! not swept away when the host's session ends. This program follows the same
//! shape for the effect queue.
//!
//! ## What it does
//!
//! Every tick it reads the effect logs under `.rpi/superagent-effects/`, runs
//! whatever is due, and writes the outcome back. Because the rows live in files
//! rather than in a host's memory, the work survives one scheduler dying and a
//! different one starting later.
//!
//! ## Single instance
//!
//! One scheduler per state directory, enforced by an exclusive lock. A second
//! process finds the lock taken and exits: two schedulers would run the same row
//! twice.

fn main() {
    // Detach first: everything after this must survive the host's session ending.
    rpi_superagent::supervisor::detach();

    let Some(root) = rpi_superagent::passes_daemon::root_dir() else {
        eprintln!("rpi-superagent-passes: no working directory to serve");
        std::process::exit(1);
    };
    if rpi_superagent::passes_daemon::create_root(&root).is_err() {
        eprintln!(
            "rpi-superagent-passes: cannot create {}",
            root.display()
        );
        std::process::exit(1);
    }

    // Only one scheduler per root: two would run the same row twice.
    let Some(_lock) = rpi_superagent::passes_daemon::take_lock(&root) else {
        // Another scheduler already holds it; the work is theirs.
        std::process::exit(0);
    };
    // The lock lives as long as the process: the kernel releases it on exit.
    std::mem::forget(_lock);

    rpi_superagent::passes_daemon::serve(&root);
}
