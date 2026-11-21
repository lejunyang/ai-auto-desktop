//! Run the fork in its own test process: the child inherits every open lease.
#![cfg(unix)]

use aad_control::lease::InputLease;

#[test]
fn a_dropped_lease_is_free_once_a_child_forked_meanwhile_has_execed() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let root = std::env::temp_dir().join(format!("aad-lease-test-{}", uuid::Uuid::new_v4()));
    std::env::set_var("AAD_CONTROL_LEASE_DIR", &root);
    let lease = InputLease::acquire("desktop").unwrap();
    let (mut forked, signal) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd = signal.as_raw_fd();
    let mut child = std::process::Command::new("true");
    unsafe {
        child.pre_exec(move || {
            libc::write(fd, b"f".as_ptr().cast(), 1);
            libc::sleep(1);
            Ok(())
        });
    }
    let forker = std::thread::spawn(move || {
        let status = child.status().unwrap();
        drop(signal);
        status
    });
    forked.read_exact(&mut [0]).unwrap();
    drop(lease);
    assert!(InputLease::acquire("desktop").is_err());
    assert!(forker.join().unwrap().success());
    drop(InputLease::acquire("desktop").unwrap());
    std::fs::remove_dir_all(root).unwrap();
}
