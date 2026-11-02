//! Wake the follow loop when a watched directory changes.
//!
//! On Linux, [`notify::RecommendedWatcher`] is an inotify watch. The parent
//! directory of each file is watched so rotation (`rename` + create) and
//! appends both wake us. If the watcher cannot be created, or an event is
//! lost, the caller still polls on the `-s` interval.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

pub struct Watch {
    /// Held so the inotify watches stay registered. Not read after setup.
    _watcher: Option<notify::RecommendedWatcher>,
    rx: Option<Receiver<notify::Result<notify::Event>>>,
}

impl Watch {
    pub fn new(paths: &[PathBuf]) -> Self {
        let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
        match notify::RecommendedWatcher::new(tx, notify::Config::default()) {
            Ok(mut watcher) => {
                let mut dirs = HashSet::new();
                let mut warned = false;
                for path in paths {
                    let dir = parent_dir(path);
                    if !dirs.insert(dir.clone()) {
                        continue;
                    }
                    if let Err(err) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                        if !warned {
                            eprintln!(
                                "ztail: warning: cannot watch {}: {err}; polling instead",
                                dir.display()
                            );
                            warned = true;
                        }
                    }
                }
                Self {
                    _watcher: Some(watcher),
                    rx: Some(rx),
                }
            }
            Err(err) => {
                eprintln!("ztail: warning: filesystem events unavailable ({err}); polling instead");
                Self {
                    _watcher: None,
                    rx: None,
                }
            }
        }
    }

    #[cfg(test)]
    pub fn using_events(&self) -> bool {
        self._watcher.is_some()
    }

    /// Block until an event arrives, `stop` is set, or `timeout` elapses.
    ///
    /// The wait is sliced so Ctrl+C is noticed without waiting out a long
    /// `-s` interval. A timeout is not an error: the caller stats the files
    /// either way, which covers lost events and filesystems without inotify.
    pub fn wait(&self, timeout: Duration, stop: &dyn Fn() -> bool) {
        let Some(rx) = &self.rx else {
            sleep_until(timeout, stop);
            return;
        };
        let start = Instant::now();
        while start.elapsed() < timeout {
            if stop() {
                return;
            }
            let slice = (timeout - start.elapsed()).min(Duration::from_millis(200));
            match rx.recv_timeout(slice) {
                Ok(_) => {
                    while rx.try_recv().is_ok() {}
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    sleep_until(timeout.saturating_sub(start.elapsed()), stop);
                    return;
                }
            }
        }
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn sleep_until(timeout: Duration, stop: &dyn Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if stop() {
            return;
        }
        let slice = (timeout - start.elapsed()).min(Duration::from_millis(200));
        std::thread::sleep(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn modify_wakes_before_the_full_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.log");
        std::fs::write(&path, b"a\n").unwrap();
        let watch = Watch::new(std::slice::from_ref(&path));
        if !watch.using_events() {
            // Sandboxes without inotify still poll; that path is not this test.
            return;
        }
        // Drain anything produced by the create/watch itself.
        watch.wait(Duration::from_millis(50), &|| false);

        let started = Instant::now();
        let watcher = thread::spawn(move || {
            watch.wait(Duration::from_secs(5), &|| false);
        });
        thread::sleep(Duration::from_millis(100));
        std::fs::write(&path, b"a\nb\n").unwrap();
        watcher.join().unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "event wait took {elapsed:?}; inotify did not wake the follower"
        );
    }
}
