// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! The native operations the layer uses, behind a trait so host tests can
//! run the POSIX logic against a fake. Only the guest implementation makes
//! system calls; host code never does.

use deepwyrm_syscall::*;

/// One successful receive: payload bytes and handle records written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Received {
    pub bytes: usize,
    pub handles: usize,
}

pub trait Native {
    fn send(
        &mut self,
        channel: DwHandle,
        bytes: &[u8],
        transfers: &[DwHandleTransferV1],
    ) -> Result<(), DwStatus>;
    fn receive(
        &mut self,
        channel: DwHandle,
        bytes: &mut [u8],
        handles: &mut [DwReceivedHandleInfoV1],
    ) -> Result<Received, DwStatus>;
    /// Waits for any of `signals`; the full observed state.
    fn wait(
        &mut self,
        handle: DwHandle,
        signals: DwSignals,
        deadline: DwDeadline,
    ) -> Result<DwSignals, DwStatus>;
    fn close(&mut self, handle: DwHandle) -> Result<(), DwStatus>;
    fn create_channel(&mut self, rights: DwRights) -> Result<(DwHandle, DwHandle), DwStatus>;
    /// Nanoseconds on the active monotonic clock.
    fn now(&mut self) -> Result<u64, DwStatus>;
    /// Maps `length` bytes of fresh zeroed memory; the address.
    fn map_anonymous(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
        fixed: bool,
    ) -> Result<u64, DwStatus>;
    fn unmap(&mut self, root: DwHandle, address: u64, length: u64) -> Result<(), DwStatus>;
    fn protect(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
    ) -> Result<(), DwStatus>;
    fn set_fs_base(&mut self, base: u64) -> Result<(), DwStatus>;
}

#[cfg(target_os = "wyrmroot")]
fn check(status: DwStatus) -> Result<(), DwStatus> {
    if status == DW_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

/// The real kernel.
#[cfg(target_os = "wyrmroot")]
pub struct Kernel;

#[cfg(target_os = "wyrmroot")]
impl Native for Kernel {
    fn send(
        &mut self,
        channel: DwHandle,
        bytes: &[u8],
        transfers: &[DwHandleTransferV1],
    ) -> Result<(), DwStatus> {
        check(channel_send(channel, bytes, transfers, 0))
    }

    fn receive(
        &mut self,
        channel: DwHandle,
        bytes: &mut [u8],
        handles: &mut [DwReceivedHandleInfoV1],
    ) -> Result<Received, DwStatus> {
        let mut result = DwChannelReceiveResultV1::default();
        check(channel_receive(channel, bytes, handles, &mut result))?;
        let received = Received {
            bytes: result.actual_bytes as usize,
            handles: result.actual_handles as usize,
        };
        // A result the ABI does not allow is treated as a broken peer; any
        // handles it did report are closed so none leak.
        let valid = result.size == DW_CHANNEL_RECEIVE_RESULT_V1_SIZE
            && result.version == 1
            && received.bytes <= bytes.len()
            && received.handles <= handles.len();
        if valid {
            Ok(received)
        } else {
            for info in &handles[..received.handles.min(handles.len())] {
                let _ = handle_close(info.handle);
            }
            Err(DW_STATUS_BAD_STATE)
        }
    }

    fn wait(
        &mut self,
        handle: DwHandle,
        signals: DwSignals,
        deadline: DwDeadline,
    ) -> Result<DwSignals, DwStatus> {
        let mut result = DwWaitResultV1::default();
        check(wait_one(handle, signals, deadline, &mut result))?;
        Ok(result.observed)
    }

    fn close(&mut self, handle: DwHandle) -> Result<(), DwStatus> {
        check(handle_close(handle))
    }

    fn create_channel(&mut self, rights: DwRights) -> Result<(DwHandle, DwHandle), DwStatus> {
        let mut first = DwHandle(0);
        let mut second = DwHandle(0);
        check(channel_create(rights, &mut first, &mut second))?;
        Ok((first, second))
    }

    fn now(&mut self) -> Result<u64, DwStatus> {
        let mut nanoseconds = 0u64;
        check(clock_get(DW_CLOCK_MONOTONIC_ACTIVE.0, &mut nanoseconds))?;
        Ok(nanoseconds)
    }

    fn map_anonymous(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
        fixed: bool,
    ) -> Result<u64, DwStatus> {
        // The object may later be protected up to RW or RX, never both at
        // once: the ceiling is captured from these rights at map time.
        let rights =
            DwRights(DW_RIGHT_READ.0 | DW_RIGHT_WRITE.0 | DW_RIGHT_EXECUTE.0 | DW_RIGHT_MAP.0);
        let mut object = DwHandle(0);
        check(memory_object_create(
            DwSize(length),
            DwMemoryObjectCreateFlags(0),
            rights,
            &mut object,
        ))?;
        let args = DwAddressRegionMapArgsV1 {
            size: DW_ADDRESS_REGION_MAP_ARGS_V1_SIZE,
            version: DW_ADDRESS_REGION_MAP_ARGS_V1_VERSION,
            memory_object_offset: DwOffset(0),
            byte_len: DwSize(length),
            requested_address: DwUserAddress(address),
            protections,
            flags: if fixed {
                DW_ADDRESS_REGION_MAP_FLAG_FIXED
            } else {
                DwAddressRegionMapFlags(0)
            },
            ..DwAddressRegionMapArgsV1::default()
        };
        let mut mapped = DwUserAddress(0);
        let status = address_region_map(root, object, &args, &mut mapped);
        // The mapping keeps the object alive; the handle is not needed.
        let _ = handle_close(object);
        check(status)?;
        Ok(mapped.0)
    }

    fn unmap(&mut self, root: DwHandle, address: u64, length: u64) -> Result<(), DwStatus> {
        check(address_region_unmap(
            root,
            DwUserAddress(address),
            DwSize(length),
        ))
    }

    fn protect(
        &mut self,
        root: DwHandle,
        address: u64,
        length: u64,
        protections: DwMemoryProtection,
    ) -> Result<(), DwStatus> {
        check(address_region_protect(
            root,
            DwUserAddress(address),
            DwSize(length),
            protections,
        ))
    }

    fn set_fs_base(&mut self, base: u64) -> Result<(), DwStatus> {
        check(thread_set_fs_base(base))
    }
}
