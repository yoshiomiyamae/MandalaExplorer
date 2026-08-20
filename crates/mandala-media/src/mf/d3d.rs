//! The shared Direct3D 11 device that Media Foundation decodes onto.
//!
//! Without a device manager the Source Reader silently picks a software
//! decoder, which is the difference between a video costing a slice of GPU
//! decode block and costing a whole CPU core. One device is shared by every
//! stream: creating one per tile would multiply driver-side memory for no gain.

use anyhow::{Result, anyhow};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::Interface;

/// A device manager handed to every Source Reader.
///
/// Media Foundation drives the device from its own worker threads, which is
/// safe only because multithread protection is turned on below; the COM
/// pointers themselves are documented as thread-safe under that setting.
pub struct SharedDevice {
    manager: IMFDXGIDeviceManager,
    /// Held both so the device outlives the manager referencing it, and so its
    /// removal can be noticed.
    device: ID3D11Device,
}

unsafe impl Send for SharedDevice {}
unsafe impl Sync for SharedDevice {}

impl SharedDevice {
    pub fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// The device itself, for work that has to happen on the same one the
    /// decoder is using -- converting a decoded texture, above all, which is
    /// only free while the frame stays where the decoder left it.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Whether the driver still has this device.
    ///
    /// Devices are lost for reasons that have nothing to do with this app: a
    /// KVM switching monitors away, a remote desktop session attaching, a
    /// driver update, waking from sleep. Media Foundation then quietly falls
    /// back to software decoding, which reads as the app having become slow
    /// for no reason.
    pub fn is_alive(&self) -> bool {
        unsafe { self.device.GetDeviceRemovedReason().is_ok() }
    }
}

/// The device every decoder shares, made on demand and replaced when lost.
///
/// A failure here is not fatal: decoding falls back to software, which is
/// slower but still correct. Remote desktop sessions and stripped-down VMs are
/// the usual reasons for there being no device at all.
///
/// Checked on every call rather than made once, because a device that dies
/// while the app runs would otherwise keep every later decode on the CPU until
/// the app was restarted -- with nothing on screen to say why.
pub fn shared_device() -> Option<Arc<SharedDevice>> {
    static DEVICE: Mutex<Option<Arc<SharedDevice>>> = Mutex::new(None);

    let mut slot = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(device) = slot.as_ref() {
        if device.is_alive() {
            return Some(Arc::clone(device));
        }
        // Lost. Dropping it here means the streams still holding one keep it
        // alive until they notice their own failures and are torn down.
        *slot = None;
    }

    let device = Arc::new(create_device().ok()?);
    *slot = Some(Arc::clone(&device));
    Some(device)
}

fn create_device() -> Result<SharedDevice> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // VIDEO_SUPPORT is what actually unlocks the decoder; BGRA_SUPPORT
            // lets the video processor write the format the tiles want.
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut D3D_FEATURE_LEVEL::default()),
            None,
        )?;
        let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;

        // Every decoding thread shares this device, so the driver has to
        // serialize access itself. Skipping this corrupts frames under load.
        let multithread: ID3D11Multithread = device.cast()?;
        let _ = multithread.SetMultithreadProtected(true);

        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager returned none"))?;
        manager.ResetDevice(&device, token)?;

        Ok(SharedDevice { manager, device })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_device_is_reused_rather_than_remade() {
        // Also the only assertion that this machine can decode on the GPU at
        // all; a None here means every later decode quietly runs on the CPU.
        let first = shared_device();
        let second = shared_device();
        match (first, second) {
            (Some(a), Some(b)) => {
                assert!(Arc::ptr_eq(&a, &b), "a live device should be handed out again");
                assert!(a.is_alive());
            }
            (None, None) => eprintln!("no D3D11 device available; decoding will be software"),
            _ => panic!("shared_device must be consistent between calls"),
        }
    }
}
