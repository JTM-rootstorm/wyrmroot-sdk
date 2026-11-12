// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! A host fake of the native interface: Channels as queues, a clock, an
//! address space, and a small in-memory filesystem that answers WRNS and
//! WRFS the way `vfsd` and a filesystem do.

use crate::native::{Native, Received};
use deepwyrm_syscall::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use wyrmroot_fs_proto::node::{self, Attr, Body, EntryWriter, Kind};
use wyrmroot_fs_proto::{
    OPEN_CREATE, OPEN_DIRECTORY, OPEN_EXCLUSIVE, OPEN_FILE, OPEN_TRUNCATE, OPEN_WRITE, Status,
    namespace,
};

pub const NAMESPACE: DwHandle = DwHandle(0x5000);
pub const ROOT: DwHandle = DwHandle(0x6000);
/// Entries per DIRENTS page, small so listings span pages.
pub const PAGE_ENTRIES: usize = 2;

#[derive(Clone, Debug)]
pub enum FsNode {
    Directory,
    File(Vec<u8>),
}

struct OpenNode {
    path: Vec<u8>,
    write: bool,
}

type Queue = VecDeque<(Vec<u8>, Vec<DwReceivedHandleInfoV1>)>;

#[derive(Default)]
pub struct Fake {
    queues: HashMap<u64, Queue>,
    after_send: HashMap<u64, Vec<u8>>,
    release_after_send: HashSet<u64>,
    sent: HashMap<u64, Vec<Vec<u8>>>,
    capacity: HashMap<u64, usize>,
    peer_closed: HashSet<u64>,
    peers: HashMap<u64, u64>,
    closed: Vec<DwHandle>,
    next_handle: u64,
    pub clock: u64,
    pub fs: Option<BTreeMap<Vec<u8>, FsNode>>,
    nodes: HashMap<u64, OpenNode>,
    pub mappings: Vec<(u64, u64, DwMemoryProtection)>,
    next_address: u64,
    pub fs_base: u64,
    pub syncs: usize,
}

impl Fake {
    /// A fake with an empty root directory behind [`NAMESPACE`].
    pub fn with_filesystem() -> Self {
        let mut fs = BTreeMap::new();
        fs.insert(b"/".to_vec(), FsNode::Directory);
        Self {
            fs: Some(fs),
            ..Self::default()
        }
    }

    pub fn sent(&self, handle: DwHandle) -> Vec<Vec<u8>> {
        self.sent.get(&handle.0).cloned().unwrap_or_default()
    }

    pub fn send_capacity(&mut self, handle: DwHandle, records: usize) {
        self.capacity.insert(handle.0, records);
    }

    pub fn close_peer(&mut self, handle: DwHandle) {
        self.peer_closed.insert(handle.0);
    }

    pub fn queue(&mut self, handle: DwHandle, bytes: Vec<u8>) {
        self.queues
            .entry(handle.0)
            .or_default()
            .push_back((bytes, Vec::new()));
    }

    pub fn queue_with_handle(&mut self, handle: DwHandle, bytes: Vec<u8>, carried: DwHandle) {
        let info = DwReceivedHandleInfoV1 {
            handle: carried,
            ..DwReceivedHandleInfoV1::default()
        };
        self.queues
            .entry(handle.0)
            .or_default()
            .push_back((bytes, vec![info]));
    }

    pub fn queue_init(
        &mut self,
        handle: DwHandle,
        bytes: Vec<u8>,
        handles: Vec<DwReceivedHandleInfoV1>,
    ) {
        self.queues
            .entry(handle.0)
            .or_default()
            .push_back((bytes, handles));
    }

    /// `bytes` becomes receivable on `handle` after the next send on it.
    pub fn queue_after_send(&mut self, handle: DwHandle, bytes: Vec<u8>) {
        self.after_send.insert(handle.0, bytes);
    }

    /// The peer closes `handle`'s other end right after the next send on it,
    /// as init does once it has READY.
    pub fn release_after_send(&mut self, handle: DwHandle) {
        self.release_after_send.insert(handle.0);
    }

    pub fn closed(&self) -> &[DwHandle] {
        &self.closed
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        match self.fs.as_ref()?.get(path.as_bytes())? {
            FsNode::File(bytes) => Some(bytes.clone()),
            FsNode::Directory => None,
        }
    }

    pub fn put(&mut self, path: &str, node: FsNode) {
        self.fs
            .as_mut()
            .unwrap()
            .insert(path.as_bytes().to_vec(), node);
    }

    fn readable(&self, handle: u64) -> bool {
        self.queues
            .get(&handle)
            .is_some_and(|queue| !queue.is_empty())
    }

    fn reply(&mut self, client: u64, transaction_id: u64, body: Body<'_>) {
        let mut bytes = vec![0u8; node::MAX_BYTES];
        let size = node::encode(
            &node::Message {
                transaction_id,
                body,
            },
            &mut bytes,
        )
        .unwrap();
        bytes.truncate(size);
        self.queue(DwHandle(client), bytes);
    }

    fn attr(&self, path: &[u8], write: bool) -> Option<Attr> {
        let node = self.fs.as_ref()?.get(path)?;
        let access = node::ACCESS_READ | if write { node::ACCESS_WRITE } else { 0 };
        let id = path.iter().fold(7u64, |hash, &byte| {
            hash.wrapping_mul(31).wrapping_add(u64::from(byte))
        });
        Some(match node {
            FsNode::Directory => Attr {
                kind: Kind::Directory,
                access,
                size: 0,
                id,
            },
            FsNode::File(bytes) => Attr {
                kind: Kind::File,
                access,
                size: bytes.len() as u64,
                id,
            },
        })
    }

    fn open(&mut self, flags: u32, path: &[u8]) -> Result<Attr, Status> {
        let fs = self.fs.as_mut().unwrap();
        let parent = match path.iter().rposition(|&byte| byte == b'/') {
            Some(0) => b"/".to_vec(),
            Some(slash) => path[..slash].to_vec(),
            None => return Err(Status::InvalidPath),
        };
        match fs.get_mut(path) {
            Some(_) if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 => {
                return Err(Status::Exists);
            }
            Some(FsNode::File(_)) if flags & OPEN_DIRECTORY != 0 => {
                return Err(Status::NotDirectory);
            }
            Some(FsNode::Directory) if flags & OPEN_FILE != 0 => return Err(Status::IsDirectory),
            Some(FsNode::File(bytes)) if flags & OPEN_TRUNCATE != 0 => bytes.clear(),
            Some(_) => {}
            None if flags & OPEN_CREATE != 0 => match fs.get(&parent) {
                Some(FsNode::Directory) => {
                    let node = if flags & OPEN_DIRECTORY != 0 {
                        FsNode::Directory
                    } else {
                        FsNode::File(Vec::new())
                    };
                    fs.insert(path.to_vec(), node);
                }
                Some(FsNode::File(_)) => return Err(Status::NotDirectory),
                None => return Err(Status::NotFound),
            },
            None => return Err(Status::NotFound),
        }
        Ok(self.attr(path, flags & OPEN_WRITE != 0).unwrap())
    }

    fn children(&self, path: &[u8]) -> Vec<(Kind, Vec<u8>)> {
        let prefix = if path == b"/" {
            b"/".to_vec()
        } else {
            [path, b"/"].concat()
        };
        self.fs
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|(key, node)| {
                let rest = key.strip_prefix(prefix.as_slice())?;
                if rest.is_empty() || rest.contains(&b'/') {
                    return None;
                }
                let kind = match node {
                    FsNode::Directory => Kind::Directory,
                    FsNode::File(_) => Kind::File,
                };
                Some((kind, rest.to_vec()))
            })
            .collect()
    }

    fn serve_namespace(&mut self, bytes: &[u8], transfers: &[DwHandleTransferV1]) {
        let message = namespace::parse(bytes, transfers.len()).expect("valid WRNS");
        let namespace::Body::Open { flags, path } = message.body else {
            panic!("only OPEN reaches the namespace");
        };
        let server = transfers[0].handle.0;
        let client = self.peers[&server];
        match self.open(flags, path) {
            Ok(attr) => {
                self.nodes.insert(
                    client,
                    OpenNode {
                        path: path.to_vec(),
                        write: flags & OPEN_WRITE != 0,
                    },
                );
                self.reply(
                    client,
                    message.transaction_id,
                    Body::OnOpen {
                        status: Status::Ok,
                        attr: Some(attr),
                    },
                );
            }
            Err(status) => self.reply(
                client,
                message.transaction_id,
                Body::OnOpen { status, attr: None },
            ),
        }
    }

    fn serve_node(&mut self, client: u64, bytes: &[u8]) {
        let message = node::parse(bytes, 0).expect("valid WRFS");
        let id = message.transaction_id;
        let path = self.nodes[&client].path.clone();
        let write = self.nodes[&client].write;
        let status = |status| Body::Status { status, value: 0 };
        match message.body {
            Body::GetAttr => {
                let attr = self.attr(&path, write).unwrap();
                self.reply(client, id, Body::Attr(attr));
            }
            Body::Read { offset, length } => {
                let data = match self.fs.as_ref().unwrap().get(&path) {
                    Some(FsNode::File(bytes)) => {
                        let start = (offset as usize).min(bytes.len());
                        let end = (start + length as usize).min(bytes.len());
                        bytes[start..end].to_vec()
                    }
                    _ => return self.reply(client, id, status(Status::IsDirectory)),
                };
                self.reply(client, id, Body::Data(&data));
            }
            Body::Write { offset, data } => {
                if !write {
                    return self.reply(client, id, status(Status::AccessDenied));
                }
                let Some(FsNode::File(bytes)) = self.fs.as_mut().unwrap().get_mut(&path) else {
                    return self.reply(client, id, status(Status::IsDirectory));
                };
                let end = offset as usize + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[offset as usize..end].copy_from_slice(data);
                let written = data.len() as u64;
                self.reply(
                    client,
                    id,
                    Body::Status {
                        status: Status::Ok,
                        value: written,
                    },
                );
            }
            Body::Truncate { size } => {
                if let Some(FsNode::File(bytes)) = self.fs.as_mut().unwrap().get_mut(&path) {
                    bytes.resize(size as usize, 0);
                }
                self.reply(client, id, status(Status::Ok));
            }
            Body::ReadDir { cookie } => {
                let children = self.children(&path);
                let start = cookie as usize;
                let end = (start + PAGE_ENTRIES).min(children.len());
                let mut buffer = vec![0u8; node::MAX_DIRENT_BYTES];
                let mut writer = EntryWriter::new(&mut buffer);
                for (kind, name) in &children[start.min(end)..end] {
                    assert!(writer.push(*kind, name));
                }
                let entries = writer.entries();
                let next_cookie = if end < children.len() { end as u64 } else { 0 };
                let mut bytes = vec![0u8; node::MAX_BYTES];
                let size = node::encode(
                    &node::Message {
                        transaction_id: id,
                        body: Body::Dirents {
                            next_cookie,
                            entries,
                        },
                    },
                    &mut bytes,
                )
                .unwrap();
                bytes.truncate(size);
                self.queue(DwHandle(client), bytes);
            }
            Body::Unlink { name } => {
                let full = if path == b"/" {
                    [b"/", name].concat()
                } else {
                    [&path[..], b"/", name].concat()
                };
                let result = match self.fs.as_ref().unwrap().get(&full) {
                    None => Status::NotFound,
                    Some(FsNode::Directory) if !self.children(&full).is_empty() => Status::NotEmpty,
                    Some(_) => {
                        self.fs.as_mut().unwrap().remove(&full);
                        Status::Ok
                    }
                };
                self.reply(client, id, status(result));
            }
            Body::Sync => {
                self.syncs += 1;
                self.reply(client, id, status(Status::Ok));
            }
            _ => self.reply(client, id, status(Status::NotSupported)),
        }
    }
}

impl Native for Fake {
    fn send(
        &mut self,
        channel: DwHandle,
        bytes: &[u8],
        transfers: &[DwHandleTransferV1],
    ) -> Result<(), DwStatus> {
        if self.peer_closed.contains(&channel.0) {
            return Err(DW_STATUS_PEER_CLOSED);
        }
        if let Some(capacity) = self.capacity.get_mut(&channel.0) {
            if *capacity == 0 {
                return Err(DW_STATUS_WOULD_BLOCK);
            }
            *capacity -= 1;
        }
        if channel == NAMESPACE && self.fs.is_some() {
            self.serve_namespace(bytes, transfers);
        } else if self.nodes.contains_key(&channel.0) {
            self.serve_node(channel.0, bytes);
        } else {
            self.sent.entry(channel.0).or_default().push(bytes.to_vec());
        }
        if let Some(after) = self.after_send.remove(&channel.0) {
            self.queue(channel, after);
        }
        if self.release_after_send.remove(&channel.0) {
            self.peer_closed.insert(channel.0);
        }
        Ok(())
    }

    fn receive(
        &mut self,
        channel: DwHandle,
        bytes: &mut [u8],
        handles: &mut [DwReceivedHandleInfoV1],
    ) -> Result<Received, DwStatus> {
        match self
            .queues
            .get_mut(&channel.0)
            .and_then(VecDeque::pop_front)
        {
            Some((message, carried)) => {
                if message.len() > bytes.len() || carried.len() > handles.len() {
                    return Err(DW_STATUS_BUFFER_TOO_SMALL);
                }
                bytes[..message.len()].copy_from_slice(&message);
                handles[..carried.len()].copy_from_slice(&carried);
                Ok(Received {
                    bytes: message.len(),
                    handles: carried.len(),
                })
            }
            None if self.peer_closed.contains(&channel.0) => Err(DW_STATUS_PEER_CLOSED),
            None => Err(DW_STATUS_WOULD_BLOCK),
        }
    }

    fn wait(
        &mut self,
        handle: DwHandle,
        signals: DwSignals,
        deadline: DwDeadline,
    ) -> Result<DwSignals, DwStatus> {
        let mut observed = 0u64;
        if self.readable(handle.0) {
            observed |= DW_SIGNAL_READABLE.0;
        }
        if self.peer_closed.contains(&handle.0) {
            observed |= DW_SIGNAL_PEER_CLOSED.0;
        } else if self.capacity.get(&handle.0).is_none_or(|&left| left > 0) {
            observed |= DW_SIGNAL_WRITABLE.0;
        }
        if observed & signals.0 != 0 {
            return Ok(DwSignals(observed));
        }
        assert_ne!(
            deadline, DW_DEADLINE_INFINITE,
            "the fake would wait forever"
        );
        self.clock = self.clock.max(deadline.0);
        Err(DW_STATUS_TIMED_OUT)
    }

    fn close(&mut self, handle: DwHandle) -> Result<(), DwStatus> {
        self.closed.push(handle);
        self.nodes.remove(&handle.0);
        Ok(())
    }

    fn create_channel(&mut self, _rights: DwRights) -> Result<(DwHandle, DwHandle), DwStatus> {
        self.next_handle += 2;
        let first = 0x10_0000 + self.next_handle;
        let second = first + 1;
        self.peers.insert(first, second);
        self.peers.insert(second, first);
        Ok((DwHandle(first), DwHandle(second)))
    }

    fn now(&mut self) -> Result<u64, DwStatus> {
        Ok(self.clock)
    }

    fn map_anonymous(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
        fixed: bool,
    ) -> Result<u64, DwStatus> {
        assert_eq!(root, ROOT);
        let start = if fixed {
            if self
                .mappings
                .iter()
                .any(|&(base, size, _)| address < base + size && base < address + length)
            {
                return Err(DW_STATUS_ALREADY_EXISTS);
            }
            address
        } else {
            self.next_address = self.next_address.max(0x4000_0000);
            let start = self.next_address;
            self.next_address += length;
            start
        };
        self.mappings.push((start, length, protections));
        Ok(start)
    }

    fn unmap(&mut self, root: DwHandle, address: u64, length: u64) -> Result<(), DwStatus> {
        assert_eq!(root, ROOT);
        let before = self.mappings.len();
        self.mappings
            .retain(|&(base, size, _)| !(base == address && size == length));
        if self.mappings.len() == before {
            Err(DW_STATUS_INVALID_ARGUMENT)
        } else {
            Ok(())
        }
    }

    fn protect(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
    ) -> Result<(), DwStatus> {
        assert_eq!(root, ROOT);
        for mapping in &mut self.mappings {
            if mapping.0 == address && mapping.1 == length {
                mapping.2 = protections;
                return Ok(());
            }
        }
        Err(DW_STATUS_NOT_FOUND)
    }

    fn set_fs_base(&mut self, base: u64) -> Result<(), DwStatus> {
        if base >= 0x0000_8000_0000_0000 {
            return Err(DW_STATUS_INVALID_ARGUMENT);
        }
        self.fs_base = base;
        Ok(())
    }
}
