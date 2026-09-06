#![no_main]

use std::os::fd::AsRawFd;

use libfuzzer_sys::fuzz_target;
use touchbar_protocol::broker_ipc::{MAX_BROKER_PACKET_BYTES, Seqpacket};

fn send_raw(socket: &Seqpacket, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    // SAFETY: bytes is readable and socket is a live local seqpacket endpoint.
    unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        ) == bytes.len() as isize
    }
}

fuzz_target!(|input: &[u8]| {
    let bytes = &input[..input.len().min(MAX_BROKER_PACKET_BYTES + 1)];

    let (sender, receiver) = Seqpacket::pair().unwrap();
    if send_raw(&sender, bytes) {
        let _ = receiver.recv_host();
    }

    let (sender, receiver) = Seqpacket::pair().unwrap();
    if send_raw(&sender, bytes) {
        let _ = receiver.recv_supervisor();
    }
});
