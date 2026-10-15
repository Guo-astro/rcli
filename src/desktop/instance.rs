//! The one Claude Desktop instance a `wally claude-desktop` run started, by
//! process id, so the two can end together: the app quitting ends the run, and
//! the run ending (Ctrl-C) quits the app.
//!
//! `open -n` starts the instance through launchd, so it is not wally's child
//! and no pid comes back. It is found instead as the main process that was not
//! running before the launch.

use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

/// The pids of `bundle`'s main processes: executables in `Contents/MacOS`.
/// Helpers live under `Contents/Frameworks` and are left out, so this names
/// the process whose exit is the app's.
pub fn main_pids(bundle: &str) -> HashSet<i32> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,comm="])
        .output()
    else {
        return HashSet::new();
    };
    let prefix = format!("{}/Contents/MacOS/", bundle.trim_end_matches('/'));
    parse_main_pids(&String::from_utf8_lossy(&output.stdout), &prefix)
}

/// `ps -axo pid=,comm=` lines whose command starts with `prefix`.
fn parse_main_pids(listing: &str, prefix: &str) -> HashSet<i32> {
    listing
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            command
                .trim_start()
                .starts_with(prefix)
                .then(|| pid.parse().ok())
                .flatten()
        })
        .collect()
}

/// The main process of `bundle` that is not in `before`, waiting up to
/// `within` for it to appear. None when no new one did: `open` handed the
/// launch to an instance that was already running.
pub fn wait_for_new(bundle: &str, before: &HashSet<i32>, within: Duration) -> Option<i32> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(pid) = main_pids(bundle).difference(before).min() {
            return Some(*pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn is_running(pid: i32) -> bool {
    // SAFETY: kill with signal 0 sends nothing; it only checks that `pid`
    // exists and may be signalled.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Blocks until `pid` has exited.
pub fn wait_for_exit(pid: i32) {
    while is_running(pid) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Asks `pid` to quit (SIGTERM, which Electron treats as a normal quit) and
/// waits up to `grace` for it to go. True once it has.
pub fn quit(pid: i32, grace: Duration) -> bool {
    // SAFETY: `pid` names the Claude Desktop instance this run started;
    // SIGTERM asks it to quit and touches no other process.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while is_running(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_main_executables_of_the_bundle_count() {
        let listing = "\
  101 /Applications/Claude.app/Contents/MacOS/Claude
  102 /Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)
 2203 /Applications/Other.app/Contents/MacOS/Other
  304 /Applications/Claude.app/Contents/MacOS/Claude
  bad line
";
        let pids = parse_main_pids(listing, "/Applications/Claude.app/Contents/MacOS/");
        assert_eq!(pids, HashSet::from([101, 304]));
    }

    #[test]
    fn a_process_that_exited_is_not_running_and_this_one_is() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(!is_running(pid));
        assert!(is_running(std::process::id() as i32));
    }

    #[test]
    fn quit_ends_a_process_that_honours_sigterm() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        // Reaped on a thread, as launchd reaps the real app, so the pid goes.
        let reaper = std::thread::spawn(move || child.wait());
        assert!(quit(pid, Duration::from_secs(5)));
        reaper.join().unwrap().unwrap();
    }
}
