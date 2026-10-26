//! Read-only chain-load of the OS bootloader (constitution §1.3:
//! LoadImage/StartImage only, never overwrite the ESP).
//!
//! Strategy: after a completed transaction, wait five seconds before probing
//! candidate loaders. Any key aborts automatic boot and returns to the firmware
//! shell. On timeout, probe every SimpleFileSystem volume in priority order —
//! Windows bootmgfw first, then the common Linux loader paths, and finally the
//! generic removable-media BOOTX64.EFI as a last resort. Because that generic
//! path is also where THIS application usually lives, the last resort keeps a
//! separate 5-second key window and excludes the self-volume by fingerprint
//! (our own image device path), because the handle-level match is not reliable
//! across firmware implementations.

use alloc::vec::Vec;
use log::info;
use uefi::boot::{self, LoadImageSource, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::proto::BootPolicy;
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileAttribute, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{cstr16, CStr16, Identify, Result};

/// Candidate OS loaders, tried in order on every SimpleFileSystem volume.
/// bootmgfw first (Windows is the overwhelmingly common target on this
/// tool's audience), Linux distro loaders next, the generic removable
/// default last.
const CANDIDATES: &[&CStr16] = &[
    cstr16!(r"\EFI\Microsoft\Boot\bootmgfw.efi"),
    cstr16!(r"\EFI\ubuntu\shimx64.efi"),
    cstr16!(r"\EFI\ubuntu\grubx64.efi"),
    cstr16!(r"\EFI\fedora\shimx64.efi"),
    cstr16!(r"\EFI\debian\shimx64.efi"),
    cstr16!(r"\EFI\arch\grubx64.efi"),
    cstr16!(r"\EFI\Manjaro\grubx64.efi"),
    cstr16!(r"\EFI\opensuse\shimx64.efi"),
    cstr16!(r"\EFI\grub\grubx64.efi"),
    cstr16!(r"\EFI\systemd\systemd-bootx64.efi"),
    cstr16!(r"\EFI\shim\shimx64.efi"),
];

/// Generic removable-media default, tried LAST with a 5 s abort window
/// (it may be this very application).
const GENERIC_LAST: &CStr16 = cstr16!(r"\EFI\BOOT\BOOTX64.EFI");

/// Device path node header: type, subtype, u16 length (includes header).
const NODE_TYPE_MEDIA: u8 = 0x04;
const NODE_SUBTYPE_FILE: u8 = 0x04;
const NODE_TYPE_END: u8 = 0x7f;
/// END_ENTIRE node: type 0x7f, subtype 0xff, length 0x0004 (little endian).
const END_ENTIRE: [u8; 4] = [0x7f, 0xff, 0x04, 0x00];

/// Give the user a five-second handoff gate after a completed transaction.
/// Timeout continues into the existing default candidate order; any key leaves
/// the application without loading another EFI image.
pub fn wait_before_default_boot() -> Result<()> {
    info!("==================================================");
    info!(" UNLOCK SUCCESS");
    info!(" GPU unlock completed; default boot flow is next.");
    info!("==================================================");
    info!(" 5s TIMEOUT: default boot starts automatically");
    info!(" PRESS ANY KEY TO ABORT AUTOMATIC BOOT");
    info!("==================================================");
    match crate::keywait::wait_any_key_or_timeout(5) {
        crate::keywait::KeyOrTimeout::Key(k) => {
            info!("CHAIN: key {:?} - automatic default boot cancelled", k);
            Err(uefi::Status::ABORTED.into())
        }
        crate::keywait::KeyOrTimeout::Timeout => {
            info!(" 5s TIMEOUT REACHED: entering default boot flow");
            Ok(())
        }
    }
}

pub fn chain_load_bootmgfw() -> Result<()> {
    info!("PHASE: CHAIN");
    // The shell's `map -r` equivalent: before enumerating filesystems,
    // recursively reconnect every handle so block devices carry freshly
    // bound FAT/SFS instances (ours may have been torn down or left in an
    // intermediate state by the detach/reconnect storm during the
    // transaction; a fresh UEFI shell re-binds and sees everything).
    if let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&crate::pci::PciIo::GUID)) {
        for h in handles.iter() {
            let _ = boot::connect_controller(*h, &[], None, true);
        }
        // Let binding callbacks settle before the SFS enumeration.
        boot::stall(core::time::Duration::from_millis(300));
    }
    let fs_handles = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleFileSystem::GUID))?;
    info!("CHAIN: {} SimpleFileSystem handles", fs_handles.len());

    // Our own volume is fingerprinted per volume inside open_volume_logged
    // (device-path prefix + loaded-image device match; the plain handle
    // comparison does not hold on every firmware).

    // Pass 1: specific candidates in GLOBAL priority order - bootmgfw on
    // ANY volume first (the normal Windows chain), then the common Linux
    // loaders. Our own volume is not excluded here: on a Windows install
    // stick bootmgfw lives on our own volume and is the correct target.
    let mut missed = alloc::vec![0u8; fs_handles.len()];
    for path in CANDIDATES {
        for (i, handle) in fs_handles.iter().enumerate() {
            let Some((mut dir, _)) = open_volume_logged(*handle) else {
                continue;
            };
            if dir.open(path, FileMode::Read, FileAttribute::empty()).is_ok() {
                info!("CHAIN: loader found on {:?}: {}", handle, path);
                drop(dir);
                return start_loader(*handle, path);
            }
            missed[i] = missed[i].saturating_add(1);
        }
    }
    for (i, handle) in fs_handles.iter().enumerate() {
        if missed[i] == 0 {
            continue; // already logged as unopenable during the scan
        }
        if let Some((_, is_own)) = open_volume_logged(*handle) {
            info!(
                "CHAIN: no specific candidate on {:?} (own={}, probes={})",
                handle,
                is_own,
                missed[i]
            );
        }
    }

    // Pass 2: the generic removable default, with a 5 s abort window. The
    // self-volume is excluded by fingerprint, so a stick that only contains
    // us does not relaunch itself; the user gets the shell instead. Doing
    // nothing runs the generic loader (the desired default for "whatever
    // the stick was set up to boot").
    for handle in fs_handles.iter() {
        let Some((mut dir, is_own)) = open_volume_logged(*handle) else {
            continue;
        };
        if is_own {
            info!("CHAIN: generic fallback skipped on own volume {:?}", handle);
            continue;
        }
        if dir.open(GENERIC_LAST, FileMode::Read, FileAttribute::empty()).is_err() {
            info!("CHAIN: no generic loader on {:?} (own={})", handle, is_own);
            continue;
        }
        info!("CHAIN: generic loader present on {:?}: {}", handle, GENERIC_LAST);
        drop(dir);
        info!("CHAIN: fallback in 5 s - press ANY KEY to abort to the firmware shell");
        info!("CHAIN: unlock is COMPLETE and logged; the GPU is in a normal cold state -");
        info!("CHAIN: after returning to the shell you may start any OS normally.");
        match crate::keywait::wait_any_key_or_timeout(5) {
            crate::keywait::KeyOrTimeout::Key(k) => {
                info!("CHAIN: key {:?} - aborting to firmware (user choice)", k);
                return Err(uefi::Status::ABORTED.into());
            }
            crate::keywait::KeyOrTimeout::Timeout => info!("CHAIN: 5 s timeout - launching the generic loader"),
        }
        return start_loader(*handle, GENERIC_LAST);
    }

    info!("CHAIN: no OS loader found on any filesystem.");
    info!("CHAIN: unlock is COMPLETE and logged - the GPU is in a normal cold state.");
    info!("CHAIN: boot the machine into its UEFI boot menu / built-in shell and");
    info!("CHAIN: start the desired OS manually; no further tool action is needed.");
    Err(uefi::Status::NOT_FOUND.into())
}

/// Open the volume with one retry (freshly reconnected volumes may need a
/// beat), and report whether this handle is our own volume (device-path
/// fingerprint: our image path's file-name tail is compared against the
/// volume's device path, because the device-handle match is not reliable).
fn open_volume_logged(
    handle: uefi::Handle,
) -> Option<(uefi::proto::media::file::Directory, bool)> {
    // Safety: ByHandleProtocol, protocol lives as long as the call.
    let fs = unsafe {
        boot::open_protocol::<SimpleFileSystem>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    };
    let mut fs = match fs {
        Ok(fs) => fs,
        Err(e) => {
            info!("CHAIN: open fs {:?} failed: {:?}", handle, e.status());
            return None;
        }
    };
    let is_own = own_volume_by_probe(handle);
    let dir = match fs.open_volume() {
        Ok(d) => d,
        Err(e1) => {
            boot::stall(core::time::Duration::from_millis(200));
            match fs.open_volume() {
                Ok(d) => {
                    info!("CHAIN: open_volume recovered on {:?} after retry (first: {:?})", handle, e1.status());
                    d
                }
                Err(e2) => {
                    info!(
                        "CHAIN: open_volume failed on {:?}: {:?} then {:?} (own={})",
                        handle,
                        e1.status(),
                        e2.status(),
                        is_own
                    );
                    return None;
                }
            }
        }
    };
    Some((dir, is_own))
}

/// Start the loader at `path` on the volume `handle` (read-only chainload).
fn start_loader(handle: uefi::Handle, path: &'static CStr16) -> Result<()> {
    // Safety: same ByHandleProtocol discipline for the device path.
    let dp = unsafe {
        boot::open_protocol::<DevicePath>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }?;

    let full = match splice_file_node(&dp, path) {
        Some(v) => v,
        None => {
            info!("CHAIN: device path splice failed on {:?}", handle);
            return Err(uefi::Status::UNSUPPORTED.into());
        }
    };

    // Safety: `full` outlives the load_image call; firmware parses it.
    let full_dp = unsafe { DevicePath::from_ffi_ptr(full.as_ptr().cast()) };
    info!("CHAIN: LoadImage (FromDevicePath)");
    // Flush and close the USB log before handing control away; lines
    // logged after this point go to console only.
    crate::logging::finish();
    let loaded = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path: full_dp,
            boot_policy: BootPolicy::ExactMatch,
        },
    )?;
    info!("CHAIN: StartImage -> handing control to the OS loader");
    let r = boot::start_image(loaded);
    info!("CHAIN: loader returned: {:?}", r);
    Ok(())
}

/// The volume portion of our own launch device path: every node BEFORE the
/// first FilePath node. None when the firmware exposes no file path.
fn image_volume_prefix() -> Option<Vec<u8>> {
    // Safety: GetProtocol on our own image handle during boot services.
    let li = unsafe {
        boot::open_protocol::<LoadedImage>(
            OpenProtocolParams {
                handle: boot::image_handle(),
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()?;
    let path = li.file_path()?;
    let bytes = path.as_bytes();
    let mut off = 0usize;
    while off + 4 <= bytes.len() {
        let len = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) as usize;
        if len < 4 || off + len > bytes.len() {
            return None;
        }
        if bytes[off] == NODE_TYPE_MEDIA && bytes[off + 1] == NODE_SUBTYPE_FILE {
            return Some(bytes[..off].to_vec());
        }
        if bytes[off] == NODE_TYPE_END {
            return None;
        }
        off += len;
    }
    None
}

/// The device handle our image was loaded from, if the firmware reports one.
fn own_device_handle() -> Option<uefi::Handle> {
    // Safety: GetProtocol on our own image handle during boot services.
    let li = unsafe {
        boot::open_protocol::<LoadedImage>(
            OpenProtocolParams {
                handle: boot::image_handle(),
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()?;
    li.device()
}

/// The node sequence of `bytes` with the END node stripped.
fn dp_before_end(bytes: &[u8]) -> &[u8] {
    let mut off = 0usize;
    while off + 4 <= bytes.len() {
        let len = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) as usize;
        if len < 4 || off + len > bytes.len() {
            break;
        }
        if bytes[off] == NODE_TYPE_END {
            break;
        }
        off += len;
    }
    &bytes[..off]
}

/// Fingerprint check: is this volume the one our image was started from?
/// Two independent signals, because neither holds on every firmware:
/// (a) the volume device path is a byte prefix of our own launch path - this
/// also covers the case that matters, the generic BOOTX64.EFI being our own
/// file (we must never relaunch ourselves); (b) the loaded-image device
/// handle names this very handle.
fn own_volume_by_probe(handle: uefi::Handle) -> bool {
    if let Some(prefix) = image_volume_prefix() {
        if !prefix.is_empty() {
            // Safety: ByHandleProtocol discipline.
            let dp = unsafe {
                boot::open_protocol::<DevicePath>(
                    OpenProtocolParams {
                        handle,
                        agent: boot::image_handle(),
                        controller: None,
                    },
                    OpenProtocolAttributes::GetProtocol,
                )
            };
            if let Ok(dp) = dp {
                if dp_before_end(dp.as_bytes()) == prefix.as_slice() {
                    return true;
                }
            }
        }
    }
    if let Some(dev) = own_device_handle() {
        if dev == handle {
            return true;
        }
    }
    false
}

/// `base` + FilePath(path) + END_ENTIRE, as raw device-path bytes.
fn splice_file_node(base: &DevicePath, path: &CStr16) -> Option<Vec<u8>> {
    let data = base.as_bytes();
    if data.len() < 4 || data[data.len() - 4..] != END_ENTIRE {
        return None; // unexpected layout; refuse to splice
    }
    let mut out = Vec::with_capacity(data.len() + 8 + 64);
    out.extend_from_slice(&data[..data.len() - 4]); // strip END_ENTIRE

    let mut name: Vec<u16> = path.as_slice().iter().map(|c| u16::from(*c)).collect();
    name.push(0); // NUL terminator
    let node_len = (4 + 2 * name.len()) as u16;
    out.push(NODE_TYPE_MEDIA);
    out.push(NODE_SUBTYPE_FILE);
    out.extend_from_slice(&node_len.to_le_bytes());
    for u in name {
        out.extend_from_slice(&u.to_le_bytes());
    }

    out.extend_from_slice(&END_ENTIRE);
    Some(out)
}
