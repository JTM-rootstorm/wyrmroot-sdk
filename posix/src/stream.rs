// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Stream descriptors over WRST v1 Channels (root
//! `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §3).
//!
//! A write becomes ⌈n/1024⌉ DATA records and returns once they are queued;
//! a read returns what one record holds and keeps the rest for the next
//! read. Peer closure with nothing buffered is end of file on input and
//! `EPIPE` on output. A record that breaks WRST, or carries handles, fails
//! the descriptor for good.

use crate::errno::{EAGAIN, EIO, EPIPE, Result, from_status};
use crate::native::Native;
use deepwyrm_syscall::*;
use wyrmroot_stream_proto::{MAX_PAYLOAD_BYTES, MAX_RECORD_BYTES, decode_data, encode_data};

const INPUT_SIGNALS: DwSignals = DwSignals(DW_SIGNAL_READABLE.0 | DW_SIGNAL_PEER_CLOSED.0);
const OUTPUT_SIGNALS: DwSignals = DwSignals(DW_SIGNAL_WRITABLE.0 | DW_SIGNAL_PEER_CLOSED.0);
/// Received handles are refused, but the receive must have room to take
/// and close them.
const HANDLE_ROOM: usize = 4;

pub struct Input {
    pub channel: DwHandle,
    record: [u8; MAX_PAYLOAD_BYTES],
    offset: usize,
    used: usize,
    eof: bool,
    failed: bool,
}

impl Input {
    pub const fn new(channel: DwHandle) -> Self {
        Self {
            channel,
            record: [0; MAX_PAYLOAD_BYTES],
            offset: 0,
            used: 0,
            eof: false,
            failed: false,
        }
    }

    pub fn read<N: Native>(
        &mut self,
        native: &mut N,
        output: &mut [u8],
        nonblocking: bool,
    ) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.failed {
                return Err(EIO);
            }
            if self.offset != self.used {
                let count = output.len().min(self.used - self.offset);
                output[..count].copy_from_slice(&self.record[self.offset..self.offset + count]);
                self.offset += count;
                return Ok(count);
            }
            if self.eof {
                return Ok(0);
            }
            let mut wire = [0u8; MAX_RECORD_BYTES];
            let mut handles = [DwReceivedHandleInfoV1::default(); HANDLE_ROOM];
            match native.receive(self.channel, &mut wire, &mut handles) {
                Ok(received) => {
                    if received.handles != 0 {
                        for info in &handles[..received.handles] {
                            let _ = native.close(info.handle);
                        }
                        self.failed = true;
                        continue;
                    }
                    match decode_data(&wire[..received.bytes]) {
                        // An empty record is a valid no-op, never EOF.
                        Ok(data) => {
                            let payload = data.payload();
                            self.record[..payload.len()].copy_from_slice(payload);
                            self.offset = 0;
                            self.used = payload.len();
                        }
                        Err(_) => self.failed = true,
                    }
                }
                Err(DW_STATUS_PEER_CLOSED) => self.eof = true,
                Err(DW_STATUS_WOULD_BLOCK) => {
                    if nonblocking {
                        return Err(EAGAIN);
                    }
                    native
                        .wait(self.channel, INPUT_SIGNALS, DW_DEADLINE_INFINITE)
                        .map_err(from_status)?;
                }
                // Too large or otherwise unreadable: the stream is broken.
                Err(_) => self.failed = true,
            }
        }
    }

    /// Whether a read would return at once.
    pub fn buffered(&self) -> bool {
        self.offset != self.used || self.eof || self.failed
    }
}

/// Writes `bytes` to a WRST Channel as DATA records; the count queued.
pub fn write<N: Native>(
    native: &mut N,
    channel: DwHandle,
    bytes: &[u8],
    nonblocking: bool,
) -> Result<usize> {
    let mut committed = 0usize;
    while committed != bytes.len() {
        let packet = (bytes.len() - committed).min(MAX_PAYLOAD_BYTES);
        let mut wire = [0u8; MAX_RECORD_BYTES];
        let size =
            encode_data(&bytes[committed..committed + packet], &mut wire).map_err(|_| EIO)?;
        match native.send(channel, &wire[..size], &[]) {
            Ok(()) => committed += packet,
            Err(DW_STATUS_WOULD_BLOCK) => {
                if committed != 0 {
                    return Ok(committed);
                }
                if nonblocking {
                    return Err(EAGAIN);
                }
                let observed = native
                    .wait(channel, OUTPUT_SIGNALS, DW_DEADLINE_INFINITE)
                    .map_err(from_status)?;
                if observed.0 & DW_SIGNAL_PEER_CLOSED.0 != 0 {
                    return Err(EPIPE);
                }
            }
            Err(DW_STATUS_PEER_CLOSED) if committed != 0 => return Ok(committed),
            Err(DW_STATUS_PEER_CLOSED) => return Err(EPIPE),
            Err(status) => {
                return if committed != 0 {
                    Ok(committed)
                } else {
                    Err(from_status(status))
                };
            }
        }
    }
    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fake;

    const OUT: DwHandle = DwHandle(0x10);
    const IN: DwHandle = DwHandle(0x20);

    fn record(payload: &[u8]) -> Vec<u8> {
        let mut wire = [0u8; MAX_RECORD_BYTES];
        let size = encode_data(payload, &mut wire).unwrap();
        wire[..size].to_vec()
    }

    #[test]
    fn writes_are_split_into_records_of_at_most_1024_bytes() {
        let mut fake = Fake::default();
        let bytes: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        assert_eq!(write(&mut fake, OUT, &bytes, false), Ok(2500));
        let sent = fake.sent(OUT);
        let sizes: Vec<usize> = sent
            .iter()
            .map(|wire| decode_data(wire).unwrap().payload().len())
            .collect();
        assert_eq!(sizes, [1024, 1024, 452]);
        let joined: Vec<u8> = sent
            .iter()
            .flat_map(|wire| decode_data(wire).unwrap().payload().to_vec())
            .collect();
        assert_eq!(joined, bytes);
    }

    #[test]
    fn an_empty_write_sends_nothing() {
        let mut fake = Fake::default();
        assert_eq!(write(&mut fake, OUT, &[], false), Ok(0));
        assert!(fake.sent(OUT).is_empty());
    }

    #[test]
    fn a_full_channel_returns_what_was_queued_or_waits() {
        let mut fake = Fake::default();
        fake.send_capacity(OUT, 1);
        // One record fits; the second would block, so the write is short.
        assert_eq!(write(&mut fake, OUT, &[7u8; 1500], false), Ok(1024));
        // Nonblocking with nothing queued is EAGAIN.
        assert_eq!(write(&mut fake, OUT, &[7u8; 10], true), Err(EAGAIN));
        // A closed peer is EPIPE.
        fake.close_peer(OUT);
        assert_eq!(write(&mut fake, OUT, &[7u8; 10], false), Err(EPIPE));
    }

    #[test]
    fn reads_keep_the_rest_of_a_record_for_later() {
        let mut fake = Fake::default();
        fake.queue(IN, record(b"hello, world"));
        let mut input = Input::new(IN);
        let mut buffer = [0u8; 5];
        assert_eq!(input.read(&mut fake, &mut buffer, false), Ok(5));
        assert_eq!(&buffer, b"hello");
        let mut rest = [0u8; 64];
        assert_eq!(input.read(&mut fake, &mut rest, false), Ok(7));
        assert_eq!(&rest[..7], b", world");
    }

    #[test]
    fn empty_records_are_skipped_and_peer_close_is_end_of_file() {
        let mut fake = Fake::default();
        fake.queue(IN, record(b""));
        fake.queue(IN, record(b"x"));
        fake.close_peer(IN);
        let mut input = Input::new(IN);
        let mut buffer = [0u8; 8];
        assert_eq!(input.read(&mut fake, &mut buffer, false), Ok(1));
        assert_eq!(input.read(&mut fake, &mut buffer, false), Ok(0));
        assert_eq!(input.read(&mut fake, &mut buffer, false), Ok(0));
    }

    #[test]
    fn a_nonblocking_read_with_nothing_queued_is_eagain() {
        let mut fake = Fake::default();
        let mut input = Input::new(IN);
        assert_eq!(input.read(&mut fake, &mut [0u8; 4], true), Err(EAGAIN));
    }

    #[test]
    fn malformed_records_and_handles_break_the_stream() {
        let mut fake = Fake::default();
        fake.queue(IN, b"not a record".to_vec());
        let mut input = Input::new(IN);
        assert_eq!(input.read(&mut fake, &mut [0u8; 4], false), Err(EIO));
        // Broken for good, even if good data follows.
        fake.queue(IN, record(b"ok"));
        assert_eq!(input.read(&mut fake, &mut [0u8; 4], false), Err(EIO));

        let mut fake = Fake::default();
        fake.queue_with_handle(IN, record(b"ok"), DwHandle(0x99));
        let mut input = Input::new(IN);
        assert_eq!(input.read(&mut fake, &mut [0u8; 4], false), Err(EIO));
        assert!(fake.closed().contains(&DwHandle(0x99)));
    }
}
