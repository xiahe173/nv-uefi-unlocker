//! M0 skeleton: banner -> single-key confirm (30s timeout) -> PCI enumeration
//! (read-only) -> chain-load Windows Boot Manager. Zero GPU writes by design.

#![no_main]
#![no_std]

extern crate alloc;
extern crate log;

use log::info;
use uefi::prelude::*;

mod banner;
mod chainload;
mod gop;
mod keywait;
mod logging;
mod pci;
mod sample;
#[cfg(feature = "tx")]
mod tx;
#[cfg(feature = "tx")]
mod tx_fwsec;
#[cfg(feature = "tx")]
mod tx_booter;
#[cfg(feature = "tx")]
mod tx_detach;
#[cfg(feature = "tx")]
mod tx_portal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[entry]
fn main() -> Status {
    logging::init(VERSION);
    banner::print(VERSION);

    // Phase: CONFIRM. Any key proceeds early; timeout proceeds anyway.
    // (M0 is read-only. From M3 on, timeout must mean ABORT into chain-load.)
    match keywait::wait_any_key_or_timeout(30) {
        keywait::KeyOrTimeout::Key(k) => info!("CONFIRM: key {:?} pressed, proceeding", k),
        keywait::KeyOrTimeout::Timeout => {
            info!("CONFIRM: 30s timeout, proceeding (M0 read-only default)")
        }
    }

    // Phase: ENUMERATE (read-only).
    let gpu = pci::enumerate();

    // Phase: GOP-PROBE (read-only; which console driver owns which GPU).
    gop::enumerate_gop();

    // Phase: SAMPLE (M1: read-only cold-state register sweep).
    match &gpu {
        Some(gpu) => sample::run(gpu),
        None => info!("SAMPLE: no NVIDIA GPU present, skipped (negative control path)"),
    }

    // Phase: TX (feature-gated transaction; only in explicit tx builds).
    // NOTE: inside tx_phase the console drivers are detached (TOTORONG
    // craft) - the screen goes dark from that point until Windows boots.
    // The log file keeps recording and is the evidence record.
    #[cfg(feature = "tx")]
    let tx_ready = if let Some(gpu_handle) = &gpu {
        tx_phase(gpu_handle.handle)
    } else {
        false
    };
    #[cfg(not(feature = "tx"))]
    let tx_ready = false;

    // A completed transaction gets a short, user-cancellable handoff window.
    // Failures and read-only runs keep their existing immediate chain behavior.
    if tx_ready {
        if let Err(e) = chainload::wait_before_default_boot() {
            info!("CHAIN: automatic default boot aborted: {:?}", e.status());
            logging::finish();
            return e.status();
        }
    }

    // Phase: CHAIN. Read-only LoadImage/StartImage of the OS loader selected
    // by the existing default candidate order.
    match chainload::chain_load_bootmgfw() {
        Ok(()) => info!("CHAIN: bootmgfw returned unexpectedly"),
        Err(e) => info!("CHAIN: failed: {:?}", e.status()),
    }

    info!("FINAL: read-only run complete; nothing was written to the GPU, nothing to restore. Safe to reboot.");
    logging::finish();
    Status::SUCCESS
}


#[cfg(feature = "tx")]
fn tx_phase(handle: uefi::Handle) -> bool {
    use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
    let proto = unsafe {
        boot::open_protocol::<pci::PciIo>(
            OpenProtocolParams { handle, agent: boot::image_handle(), controller: None },
            OpenProtocolAttributes::GetProtocol,
        )
    };
    let Ok(pci) = proto else {
        info!("TX: open PciIo failed");
        return false;
    };
    let gpu = tx::TxGpu { pci: &pci, bar: 0 };
    tx::list_root_entries();

    // Per-family asset selection by PCI device ID (constitution 5: no
    // cross-family payloads — a GA102 payload on AD102 HALTs and opens
    // nothing). Names on the boot volume: <asset>_<family>.bin.
    let dev = pci.config_read_u16(0x02);
    let family: &str = match dev {
        // AD10x (Ada): the full upstream ad102_family() routing table -
        // every listed ID uses the same pinned AD102 booter/payload.
        // Silicon-verified upstream: 2684/2685/2705/2805/2882/28a0/28e1;
        // verified here: 28e0; the rest are upstream-admitted IDs. 28a1 is
        // NVIDIA's other 4050 Laptop ID (same AD107M die as verified-silicon
        // 28e1, same 28xx block) but missing from upstream's table.
        // TODO(28a1): first-silicon run should re-verify checklist evidence.
        0x2684 | 0x2685 | 0x2689  // AD102 4090 / 4090D / 4070TiS
        | 0x2702 | 0x2704 | 0x2705 | 0x2709  // AD103 4080S/4080/4070TiS/4070
        | 0x2757  // AD103M 4090 Laptop
        | 0x2782 | 0x2783 | 0x2786  // AD104 4070Ti/4070S/4070
        | 0x2803 | 0x2805  // AD106 4060Ti / 4060Ti 16GB
        | 0x2882 | 0x28a0 | 0x28a1 | 0x28e0 | 0x28e1  // AD107 4060/4060L/4050L/4060M/4050M
        => "ad102",
        // GA10x (Ampere): 3090/3080/3070/3060/3050/CPM170HX...
        0x2204 | 0x2206 | 0x2484..=0x248c | 0x2501 | 0x2503 | 0x2504 | 0x2505
        | 0x2507 | 0x2508 | 0x2509 | 0x252f | 0x2544 | 0x2560 | 0x2584
        | 0x20c2 | 0x20f1 => "ga102",
        // Turing: 2060/2070/2080, 1660-lineage, RTX 20-series laptops.
        0x1e02..=0x1fff | 0x2182..=0x2192 => "tu10x",
        other => {
            info!("TX: device 10de:{other:04x} has no asset family mapping - skipping");
            return false;
        }
    };
    info!("TX: device 10de:{dev:04x} -> asset family '{family}'");

    let booter = tx::read_boot_file_fmt("\\booter_load_{}.bin", family);
    let boot_desc = tx::read_boot_file_fmt("\\gsp_boot_desc_{}.bin", family);
    let gsp_boot = tx::read_boot_file_fmt("\\gsp_boot_{}.bin", family);
    let gsp_image = tx::read_boot_file_fmt("\\fwimage_{}.bin", family);
    let stock_sig = tx::read_boot_file_fmt("\\fwsignature_{}.bin", family);
    let v67 = tx::read_boot_file_fmt("\\v67-{}-four-plm.bin", family);
    let (Some(booter), Some(gsp_boot), Some(gsp_image), Some(stock_sig)) =
        (booter, gsp_boot, gsp_image, stock_sig) else {
        info!("TX: assets missing for family '{family}' (booter_load_*/gsp_boot_*/fwimage_*/fwsignature_*) - skipping");
        return false;
    };
    if v67.is_none() {
        info!("TX: NO V67 file - chain-replay mode (no unlock writes)");
    } else {
        info!("TX: V67 present - four-write unlock mode");
    }
    if family == "tu10x" {
        // Turing chain (BOOT_DIRECT booter + WITH_LOADER FWSEC + BSI
        // scrubber semantics) is implemented but not hardware-validated;
        // entering the GA10x HS chain on TU106 would write wrong states.
        // Assets above are parsed+verified as evidence only.
        info!("TX: family 'tu10x' - Turing transaction track not enabled yet, skipping (no writes)");
        return false;
    }
    let assets = tx::TxAssets { family, booter, boot_desc, gsp_boot, gsp_image, stock_sig, v67 };
    match tx::run_transaction(&gpu, &assets, handle) {
        Ok(()) => {
            info!("SELECTOR RESULT: transaction complete");
            tx::handoff_out(&pci);
            assets.v67.is_some()
        }
        // Already-open PLM = already unlocked or not cold: skip (success
        // class, gpu-unlock-ada da96e3f), definitely no FLR - it would wipe
        // a live unlock.
        Err(tx::TxError::EAlready) => {
            info!("TX: already unlocked / not cold (EALREADY) - skipped, nothing to unwind");
            tx::handoff_out(&pci);
            true
        }
        // Warm entry with the require-D0 switch: nothing was written, the
        // card was not even reset - the clean "come back after a Restart"
        // outcome. No emergency FLR (FLR from a non-D0 state is exactly the
        // undefined case).
        Err(tx::TxError::EWarm) => {
            info!("TX: aborted on warm/resume entry before any GPU write - chain-loading unchanged Windows (use a Restart or cold boot)");
            tx::handoff_out(&pci);
            false
        }
        Err(e) => {
            info!("TX: transaction failed: {e:?} - NOT retrying this session");
            // Failure unwind: FLR wipes our half-states (engine resets, BCR,
            // DMA queues) so the Windows driver gets a cold-boot card.
            tx::emergency_flr(handle);
            tx::handoff_out(&pci);
            false
        }
    }
}
