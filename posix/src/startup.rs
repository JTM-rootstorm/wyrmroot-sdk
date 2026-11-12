// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//! Process startup for a job (root `TOOLCHAIN_POSIX_LAYER_DESIGN.md` §2,
//! Wyrmroot `WYR1_B_REGISTRY_LAUNCH_CONTRACT.md` §3/§4).
//!
//! `_start` hands over the launch Channel and startup ABI version before
//! musl runs. The INIT message must be a JobV2 profile: the self root and
//! the three streams, plus the WRNS namespace for a job launched from a
//! file. The streams become descriptors 0–2. READY then goes back to init,
//! and the program starts only after init releases the launch Channel by
//! closing its end, as the job contract requires.

use crate::errno::Result;
use crate::native::Native;
use crate::process::Process;
use deepwyrm_syscall::*;
use wyrmroot_startup_proto::STARTUP_ABI_V2;
use wyrmroot_startup_proto::{HEADER_BYTES, LaunchProfile, encode_ready_for_profile, parse_init};

/// Where startup failed; the process exits with `0x5750_0000 | step`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Failure {
    Version = 1,
    Receive = 2,
    Init = 3,
    Streams = 4,
    Ready = 5,
    Release = 6,
}

impl Failure {
    #[must_use]
    pub const fn exit_code(self) -> u32 {
        0x5750_0000 | self as u32
    }
}

const INIT_BYTES: usize = 128;
const MAX_INIT_HANDLES: usize = 5;
const RELEASE_SIGNALS: DwSignals = DwSignals(DW_SIGNAL_READABLE.0 | DW_SIGNAL_PEER_CLOSED.0);

/// Runs the launch exchange on `launch` and fills `process`. `pwd` is the
/// `PWD` environment value, if any.
pub fn start<N: Native>(
    native: &mut N,
    process: &mut Process,
    launch: DwHandle,
    version: u64,
    pwd: Option<&[u8]>,
) -> core::result::Result<(), Failure> {
    if version != STARTUP_ABI_V2 {
        return Err(Failure::Version);
    }
    let mut bytes = [0u8; INIT_BYTES];
    // One spare slot, so an INIT with too many handles is received whole
    // and refused rather than left queued.
    let mut handles = [DwReceivedHandleInfoV1::default(); MAX_INIT_HANDLES + 1];
    let received = loop {
        match native.receive(launch, &mut bytes, &mut handles) {
            Ok(received) => break received,
            Err(DW_STATUS_WOULD_BLOCK) => {
                native
                    .wait(launch, RELEASE_SIGNALS, DW_DEADLINE_INFINITE)
                    .map_err(|_| Failure::Receive)?;
            }
            Err(_) => return Err(Failure::Receive),
        }
    };
    let handles = &handles[..received.handles];
    let close_all = |native: &mut N| {
        for info in handles {
            let _ = native.close(info.handle);
        }
    };
    let profile = match handles.len() {
        4 => LaunchProfile::JobV2Streams,
        5 => LaunchProfile::JobV2Namespace,
        _ => {
            close_all(native);
            return Err(Failure::Init);
        }
    };
    let parsed = match parse_init(profile, &bytes[..received.bytes], handles) {
        Ok(parsed) => parsed,
        Err(_) => {
            close_all(native);
            return Err(Failure::Init);
        }
    };
    let installed: Result<()> = (|| {
        process.install_stream(handles[1].handle, true)?;
        process.install_stream(handles[2].handle, false)?;
        process.install_stream(handles[3].handle, false)?;
        Ok(())
    })();
    if installed.is_err() {
        close_all(native);
        return Err(Failure::Streams);
    }
    process.self_root = Some(handles[0].handle);
    process.namespace = handles.get(4).map(|info| info.handle);
    if let Some(pwd) = pwd {
        process.set_cwd_from_environment(pwd);
    }

    let mut ready = [0u8; HEADER_BYTES];
    let size = encode_ready_for_profile(profile, parsed.transaction_id, &mut ready)
        .map_err(|_| Failure::Ready)?;
    native
        .send(launch, &ready[..size], &[])
        .map_err(|_| Failure::Ready)?;
    // Init closes its end once it has accepted READY; only that clean
    // closure releases the job. Data after READY is a protocol failure.
    let observed = native
        .wait(launch, RELEASE_SIGNALS, DW_DEADLINE_INFINITE)
        .map_err(|_| Failure::Release)?;
    if observed.0 & DW_SIGNAL_READABLE.0 != 0 || observed.0 & DW_SIGNAL_PEER_CLOSED.0 == 0 {
        return Err(Failure::Release);
    }
    native.close(launch).map_err(|_| Failure::Release)?;
    Ok(())
}

/// The `PWD` value in a startup environment block, if present.
#[must_use]
pub fn find_pwd<'a>(mut environment: impl Iterator<Item = &'a [u8]>) -> Option<&'a [u8]> {
    environment.find_map(|entry| entry.strip_prefix(b"PWD="))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fake;
    use wyrmroot_startup_proto::{CHILD_CHANNEL_RIGHTS, encode_init};

    const LAUNCH: DwHandle = DwHandle(0x100);

    fn handle(value: u64, object_type: DwObjectType, rights: DwRights) -> DwReceivedHandleInfoV1 {
        DwReceivedHandleInfoV1 {
            handle: DwHandle(value),
            rights,
            object_type,
            ..DwReceivedHandleInfoV1::default()
        }
    }

    fn init(profile: LaunchProfile) -> (Vec<u8>, Vec<DwReceivedHandleInfoV1>) {
        let mut bytes = [0u8; INIT_BYTES];
        let size = encode_init(profile, 77, &mut bytes).unwrap();
        let root_rights = DwRights(DW_RIGHT_MAP.0 | DW_RIGHT_MODIFY.0 | DW_RIGHT_INSPECT.0);
        let mut handles = vec![
            handle(0x200, DW_OBJECT_TYPE_ADDRESS_REGION, root_rights),
            handle(0x201, DW_OBJECT_TYPE_CHANNEL, CHILD_CHANNEL_RIGHTS),
            handle(0x202, DW_OBJECT_TYPE_CHANNEL, CHILD_CHANNEL_RIGHTS),
            handle(0x203, DW_OBJECT_TYPE_CHANNEL, CHILD_CHANNEL_RIGHTS),
        ];
        if profile == LaunchProfile::JobV2Namespace {
            handles.push(handle(0x204, DW_OBJECT_TYPE_CHANNEL, CHILD_CHANNEL_RIGHTS));
        }
        (bytes[..size].to_vec(), handles)
    }

    #[test]
    fn a_namespace_job_gets_streams_root_namespace_and_cwd() {
        let mut fake = Fake::default();
        let (bytes, handles) = init(LaunchProfile::JobV2Namespace);
        fake.queue_init(LAUNCH, bytes, handles);
        fake.release_after_send(LAUNCH);
        let mut process = Box::new(Process::new());
        start(
            &mut fake,
            &mut process,
            LAUNCH,
            STARTUP_ABI_V2,
            Some(b"/home/u"),
        )
        .unwrap();
        assert_eq!(process.self_root, Some(DwHandle(0x200)));
        assert_eq!(process.namespace, Some(DwHandle(0x204)));
        assert_eq!(process.cwd(), b"/home/u");
        // READY answered INIT's transaction, then the launch Channel closed.
        let ready = fake.sent(LAUNCH);
        assert_eq!(ready.len(), 1);
        assert!(
            wyrmroot_startup_proto::parse_ready_for_profile(
                LaunchProfile::JobV2Namespace,
                &ready[0],
                77
            )
            .is_ok()
        );
        assert!(fake.closed().contains(&LAUNCH));
        // Descriptors 0-2 are the streams, in order.
        assert_eq!(process.write(&mut fake, 1, b"x"), Ok(1));
        assert_eq!(fake.sent(DwHandle(0x202)).len(), 1);
        assert_eq!(process.write(&mut fake, 0, b"x"), Err(crate::errno::EBADF));
    }

    #[test]
    fn a_bootfs_job_has_no_namespace_and_a_bad_pwd_is_ignored() {
        let mut fake = Fake::default();
        let (bytes, handles) = init(LaunchProfile::JobV2Streams);
        fake.queue_init(LAUNCH, bytes, handles);
        fake.release_after_send(LAUNCH);
        let mut process = Box::new(Process::new());
        start(
            &mut fake,
            &mut process,
            LAUNCH,
            STARTUP_ABI_V2,
            Some(b"/a/../b"),
        )
        .unwrap();
        assert_eq!(process.namespace, None);
        assert_eq!(process.cwd(), b"/");
    }

    #[test]
    fn a_wrong_version_or_init_is_refused_and_its_handles_closed() {
        let mut fake = Fake::default();
        let mut process = Box::new(Process::new());
        assert_eq!(
            start(&mut fake, &mut process, LAUNCH, 1, None),
            Err(Failure::Version)
        );

        let (mut bytes, handles) = init(LaunchProfile::JobV2Streams);
        bytes[0] ^= 0xff;
        fake.queue_init(LAUNCH, bytes, handles);
        assert_eq!(
            start(&mut fake, &mut process, LAUNCH, STARTUP_ABI_V2, None),
            Err(Failure::Init)
        );
        for value in 0x200..0x204 {
            assert!(fake.closed().contains(&DwHandle(value)));
        }
    }

    #[test]
    fn data_after_ready_is_not_a_release() {
        let mut fake = Fake::default();
        let (bytes, handles) = init(LaunchProfile::JobV2Streams);
        fake.queue_init(LAUNCH, bytes, handles);
        // Something readable on the launch Channel after READY.
        fake.queue_after_send(LAUNCH, b"extra".to_vec());
        let mut process = Box::new(Process::new());
        assert_eq!(
            start(&mut fake, &mut process, LAUNCH, STARTUP_ABI_V2, None),
            Err(Failure::Release)
        );
    }

    #[test]
    fn pwd_is_found_in_the_environment() {
        let environment: [&[u8]; 3] = [b"HOME=/h", b"PWD=/w", b"PATH=/bin"];
        assert_eq!(find_pwd(environment.into_iter()), Some(&b"/w"[..]));
        let none: [&[u8]; 1] = [b"PWDX=/w"];
        assert_eq!(find_pwd(none.into_iter()), None);
    }
}
