//! Mute stdout across FFI calls whose third-party dependencies print to it.
//!
//! swift-transformers logs its tokenizer fallback with a plain print, which
//! lands on stdout and corrupts `--json` output. There is no log level that
//! reaches it (same class of problem as the stderr mute in WallyMLX.swift).
//! Redirecting the descriptor around the call is the only handle. Stderr
//! stays live throughout, so the spinner and SDK diagnostics are unaffected.
//!
//! Two shapes: the RAII guard for bounded windows (load/score/unload), and a
//! deliberate process mute for teardown — the fallback also fires after the
//! result line renders, while the runtime shuts down, so the tail mute stays
//! on until the process exits.

#[cfg(unix)]
pub struct HushedStdout {
    saved: std::ffi::c_int,
}

#[cfg(unix)]
impl HushedStdout {
    /// Redirect stdout to /dev/null until the guard drops. Best-effort: when
    /// any step fails the guard stays inert and output flows as before.
    pub fn mute() -> Self {
        // SAFETY: scalar descriptor syscalls only; fds are process-global but
        // the guarded window is synchronous FFI with no output of ours.
        unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            if saved >= 0 {
                let null = libc::open(
                    c"/dev/null".as_ptr(),
                    libc::O_WRONLY,
                );
                if null >= 0 {
                    libc::dup2(null, libc::STDOUT_FILENO);
                    libc::close(null);
                } else {
                    libc::close(saved);
                    return HushedStdout { saved: -1 };
                }
            }
            HushedStdout { saved }
        }
    }
}

#[cfg(unix)]
impl Drop for HushedStdout {
    fn drop(&mut self) {
        if self.saved >= 0 {
            // SAFETY: restores the descriptor saved by mute.
            unsafe {
                libc::dup2(self.saved, libc::STDOUT_FILENO);
                libc::close(self.saved);
            }
        }
    }
}

/// No handle on other platforms: the noisy dependency is Apple-only, and the
/// local MLX path only exists there. Inert everywhere else.
#[cfg(not(unix))]
pub struct HushedStdout;

#[cfg(not(unix))]
impl HushedStdout {
    pub fn mute() -> Self {
        HushedStdout
    }
}

/// Mute stdout from here until the process exits. The saved descriptor is
/// deliberately leaked: restoring would unmute teardown, which is the window
/// being covered. Only for flows that print nothing else to stdout
/// afterwards (the local scorer renders its result line first).
#[cfg(unix)]
pub fn mute_stdout_process() {
    // SAFETY: same descriptor swap as the guard; intentionally never restored.
    unsafe {
        let saved = libc::dup(libc::STDOUT_FILENO);
        if saved >= 0 {
            let null =
                libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if null >= 0 {
                libc::dup2(null, libc::STDOUT_FILENO);
                libc::close(null);
            }
            // `saved` is deliberately never closed: that open descriptor is
            // what keeps the mute on until the process exits.
            let _ = saved;
        }
    }
}

/// Inert where the guard is: no teardown window exists there.
#[cfg(not(unix))]
pub fn mute_stdout_process() {}
