#[cfg(target_family = "unix")]
mod unix {
    pub use libc::SIGABRT;
    pub use libc::SIGALRM;
    pub use libc::SIGCHLD;
    pub use libc::SIGCONT;
    pub use libc::SIGFPE;
    pub use libc::SIGHUP;
    pub use libc::SIGILL;
    pub use libc::SIGINT;
    pub use libc::SIGKILL;
    pub use libc::SIGPIPE;
    pub use libc::SIGQUIT;
    pub use libc::SIGSEGV;
    pub use libc::SIGSTOP;
    pub use libc::SIGTERM;
    pub use libc::SIGTRAP;
    pub use libc::SIGTSTP;
    pub use libc::SIGUSR1;
    pub use libc::SIGUSR2;
    pub use libc::SIGWINCH;
}

#[cfg(target_family = "unix")]
pub use unix::*;

#[cfg(target_family = "windows")]
mod windows {
    pub use libc::SIGABRT;
    pub use libc::SIGFPE;
    pub use libc::SIGILL;
    pub use libc::SIGINT;
    pub use libc::SIGSEGV;
    pub use libc::SIGTERM;
}

#[cfg(target_family = "windows")]
pub use windows::*;
