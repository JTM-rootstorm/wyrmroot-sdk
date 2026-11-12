// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Files and directories through `vfsd` (root
//! `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §4, Wyrmroot WRNS/WRFS 1.1).
//!
//! A path opens with one WRNS OPEN on the namespace Channel, carrying the
//! server end of a new Channel; the filesystem answers ON_OPEN on the other
//! end and from then on speaks WRFS there. Every reply is parsed strictly
//! and must answer the request's transaction; anything else is `EIO`.

use crate::errno::{EIO, ENOMEM, Result, from_fs, from_status};
use crate::native::Native;
use deepwyrm_syscall::*;
use wyrmroot_fs_proto::node::{self, Attr, Body};
use wyrmroot_fs_proto::{MAX_HANDLES, Status, namespace};
use wyrmroot_startup_proto::CHILD_CHANNEL_RIGHTS;

/// Rights for a new node Channel pair: the server end is moved to `vfsd`.
const NODE_RIGHTS: DwRights = DwRights(CHILD_CHANNEL_RIGHTS.0 | DW_RIGHT_TRANSFER.0);
const REPLY_SIGNALS: DwSignals = DwSignals(DW_SIGNAL_READABLE.0 | DW_SIGNAL_PEER_CLOSED.0);

/// Request and reply buffers, kept out of the stack.
pub struct Scratch {
    request: [u8; node::MAX_BYTES],
    pub reply: [u8; node::MAX_BYTES],
    transaction: u64,
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Scratch {
    pub const fn new() -> Self {
        Self {
            request: [0; node::MAX_BYTES],
            reply: [0; node::MAX_BYTES],
            transaction: 0,
        }
    }

    fn next_transaction(&mut self) -> u64 {
        self.transaction = self.transaction.wrapping_add(1).max(1);
        self.transaction
    }
}

/// Waits for one handle-free message on `channel`; its length in
/// `scratch.reply`.
fn receive<N: Native>(native: &mut N, channel: DwHandle, scratch: &mut Scratch) -> Result<usize> {
    loop {
        let mut handles = [DwReceivedHandleInfoV1::default(); MAX_HANDLES + 1];
        match native.receive(channel, &mut scratch.reply, &mut handles) {
            Ok(received) if received.handles == 0 => return Ok(received.bytes),
            Ok(received) => {
                for info in &handles[..received.handles] {
                    let _ = native.close(info.handle);
                }
                return Err(EIO);
            }
            Err(DW_STATUS_WOULD_BLOCK) => {
                let observed = native
                    .wait(channel, REPLY_SIGNALS, DW_DEADLINE_INFINITE)
                    .map_err(from_status)?;
                if observed.0 & DW_SIGNAL_READABLE.0 == 0 {
                    return Err(EIO);
                }
            }
            Err(_) => return Err(EIO),
        }
    }
}

/// Opens canonical absolute `path` with WRFS `flags`; the node's Channel
/// and attributes.
pub fn open<N: Native>(
    native: &mut N,
    namespace_channel: DwHandle,
    scratch: &mut Scratch,
    flags: u32,
    path: &[u8],
) -> Result<(DwHandle, Attr)> {
    let transaction = scratch.next_transaction();
    let size = namespace::encode(
        &namespace::Message {
            transaction_id: transaction,
            body: namespace::Body::Open { flags, path },
        },
        &mut scratch.request,
    )
    .map_err(|_| crate::errno::EINVAL)?;
    let (client, server) = native.create_channel(NODE_RIGHTS).map_err(|status| {
        if status == DW_STATUS_NO_RESOURCES {
            crate::errno::EMFILE
        } else {
            from_status(status)
        }
    })?;
    let transfer = DwHandleTransferV1 {
        handle: server,
        requested_rights: NODE_RIGHTS,
        operation: DW_HANDLE_TRANSFER_MOVE,
        ..DwHandleTransferV1::default()
    };
    if let Err(status) = native.send(namespace_channel, &scratch.request[..size], &[transfer]) {
        let _ = native.close(client);
        let _ = native.close(server);
        return Err(if status == DW_STATUS_NO_MEMORY {
            ENOMEM
        } else {
            EIO
        });
    }
    finish_open(native, client, scratch, transaction)
}

/// Opens the node `channel` names again, with WRFS `flags`, onto a new
/// Channel: another description of the same file.
pub fn reopen<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    flags: u32,
) -> Result<DwHandle> {
    let transaction = scratch.next_transaction();
    let size = node::encode(
        &node::Message {
            transaction_id: transaction,
            body: Body::Open { flags, path: b"" },
        },
        &mut scratch.request,
    )
    .map_err(|_| crate::errno::EINVAL)?;
    let (client, server) = native.create_channel(NODE_RIGHTS).map_err(|status| {
        if status == DW_STATUS_NO_RESOURCES {
            crate::errno::EMFILE
        } else {
            from_status(status)
        }
    })?;
    let transfer = DwHandleTransferV1 {
        handle: server,
        requested_rights: NODE_RIGHTS,
        operation: DW_HANDLE_TRANSFER_MOVE,
        ..DwHandleTransferV1::default()
    };
    if native
        .send(channel, &scratch.request[..size], &[transfer])
        .is_err()
    {
        let _ = native.close(client);
        let _ = native.close(server);
        return Err(EIO);
    }
    finish_open(native, client, scratch, transaction).map(|(client, _)| client)
}

/// Waits for ON_OPEN on `client`; the Channel and attributes, or the
/// Channel closed and the error.
fn finish_open<N: Native>(
    native: &mut N,
    client: DwHandle,
    scratch: &mut Scratch,
    transaction: u64,
) -> Result<(DwHandle, Attr)> {
    let outcome = receive(native, client, scratch).and_then(|used| {
        match node::parse(&scratch.reply[..used], 0) {
            Ok(message) if message.transaction_id != transaction => Err(EIO),
            Ok(node::Message {
                body:
                    Body::OnOpen {
                        status: Status::Ok,
                        attr: Some(attr),
                    },
                ..
            }) => Ok(attr),
            Ok(node::Message {
                body: Body::OnOpen { status, .. },
                ..
            }) => Err(from_fs(status)),
            _ => Err(EIO),
        }
    });
    match outcome {
        Ok(attr) => Ok((client, attr)),
        Err(error) => {
            let _ = native.close(client);
            Err(error)
        }
    }
}

/// Sends one WRFS request on `channel`; the reply's length in
/// `scratch.reply`, already checked to answer this request.
pub fn request<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    body: Body<'_>,
) -> Result<usize> {
    let transaction = scratch.next_transaction();
    let size = node::encode(
        &node::Message {
            transaction_id: transaction,
            body,
        },
        &mut scratch.request,
    )
    .map_err(|_| crate::errno::EINVAL)?;
    native
        .send(channel, &scratch.request[..size], &[])
        .map_err(|_| EIO)?;
    let used = receive(native, channel, scratch)?;
    match node::parse(&scratch.reply[..used], 0) {
        Ok(message) if message.transaction_id == transaction => Ok(used),
        _ => Err(EIO),
    }
}

/// A request answered by STATUS; its value on OK.
pub fn status<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    body: Body<'_>,
) -> Result<u64> {
    let used = request(native, channel, scratch, body)?;
    match node::parse(&scratch.reply[..used], 0).map(|message| message.body) {
        Ok(Body::Status {
            status: Status::Ok,
            value,
        }) => Ok(value),
        Ok(Body::Status { status, .. }) => Err(from_fs(status)),
        _ => Err(EIO),
    }
}

pub fn get_attr<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
) -> Result<Attr> {
    let used = request(native, channel, scratch, Body::GetAttr)?;
    match node::parse(&scratch.reply[..used], 0).map(|message| message.body) {
        Ok(Body::Attr(attr)) => Ok(attr),
        Ok(Body::Status { status, .. }) => Err(from_fs(status)),
        _ => Err(EIO),
    }
}

/// Reads at most [`node::MAX_IO_BYTES`] at `offset` into `output`; the count,
/// zero at the end of the file.
pub fn read<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    offset: u64,
    output: &mut [u8],
) -> Result<usize> {
    let length = output.len().min(node::MAX_IO_BYTES);
    if length == 0 {
        return Ok(0);
    }
    let used = request(
        native,
        channel,
        scratch,
        Body::Read {
            offset,
            length: length as u32,
        },
    )?;
    match node::parse(&scratch.reply[..used], 0).map(|message| message.body) {
        Ok(Body::Data(data)) if data.len() <= length => {
            output[..data.len()].copy_from_slice(data);
            Ok(data.len())
        }
        Ok(Body::Status { status, .. }) => Err(from_fs(status)),
        _ => Err(EIO),
    }
}

/// Writes all of `data` at `offset`, in requests of at most
/// [`node::MAX_IO_BYTES`]; the count written, short only if the filesystem
/// stops early after some progress.
pub fn write<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    offset: u64,
    data: &[u8],
) -> Result<usize> {
    let mut done = 0usize;
    while done < data.len() {
        let chunk = &data[done..data.len().min(done + node::MAX_IO_BYTES)];
        let position = offset.checked_add(done as u64).ok_or(crate::errno::EFBIG)?;
        let written = match status(
            native,
            channel,
            scratch,
            Body::Write {
                offset: position,
                data: chunk,
            },
        ) {
            Ok(written) => written,
            Err(_) if done != 0 => return Ok(done),
            Err(error) => return Err(error),
        };
        let written = usize::try_from(written).map_err(|_| EIO)?;
        if written > chunk.len() {
            return Err(EIO);
        }
        done += written;
        if written < chunk.len() {
            break;
        }
    }
    Ok(done)
}

/// One DIRENTS page from `cookie`: `each` sees every entry, and the next
/// cookie (zero at the end) comes back.
pub fn read_dir<N: Native>(
    native: &mut N,
    channel: DwHandle,
    scratch: &mut Scratch,
    cookie: u64,
    mut each: impl FnMut(node::Kind, &[u8]),
) -> Result<u64> {
    let used = request(native, channel, scratch, Body::ReadDir { cookie })?;
    match node::parse(&scratch.reply[..used], 0).map(|message| message.body) {
        Ok(Body::Dirents {
            next_cookie,
            entries,
        }) => {
            for (kind, name) in entries.iter() {
                each(kind, name);
            }
            // An empty page always ends the listing.
            Ok(if entries.is_empty() { 0 } else { next_cookie })
        }
        Ok(Body::Status { status, .. }) => Err(from_fs(status)),
        _ => Err(EIO),
    }
}
