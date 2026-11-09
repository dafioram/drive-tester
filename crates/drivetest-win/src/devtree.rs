//! Plug-and-play device tree lookups: which device IDs sit above each disk.
//!
//! Used to identify USB bridges (VID / PID) and SD hosts, and to read an SD
//! card's identity from its `SD\VID_..&OID_..&PID_..&REV_..` device ID.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_DISK;

use crate::enumerate::device_number;
use crate::handle::{from_wide, open};

/// Disk number → device IDs from the disk up through up to five parents.
pub fn device_id_chains() -> HashMap<u32, Vec<String>> {
    let mut out = HashMap::new();
    // SAFETY: standard SetupAPI enumeration; every buffer is sized as the API
    // reports and outlives the calls that use it.
    unsafe {
        let set =
            SetupDiGetClassDevsW(&GUID_DEVINTERFACE_DISK, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set == -1 || set == 0 {
            return out;
        }
        for index in 0..256u32 {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &GUID_DEVINTERFACE_DISK, index, &mut iface) == 0 {
                break;
            }
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &iface, null_mut(), 0, &mut needed, null_mut());
            if needed == 0 || needed > 64 * 1024 {
                continue;
            }
            // u32 storage keeps the detail structure correctly aligned.
            let mut storage = vec![0u32; (needed as usize).div_ceil(4) + 1];
            let detail = storage.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, needed, null_mut(), &mut info) == 0 {
                continue;
            }
            let path_at = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let chars = (storage.len() * 4 - path_at) / 2;
            let path_ptr = storage.as_ptr().cast::<u8>().add(path_at).cast::<u16>();
            let path = from_wide(std::slice::from_raw_parts(path_ptr, chars));
            let Ok(h) = open(&path, 0, 0) else { continue };
            let Ok(number) = device_number(&h) else { continue };

            let mut chain = Vec::new();
            let mut node = info.DevInst;
            for _ in 0..6 {
                let mut id = [0u16; 512];
                if CM_Get_Device_IDW(node, id.as_mut_ptr(), id.len() as u32, 0) == CR_SUCCESS {
                    chain.push(from_wide(&id));
                }
                let mut parent = 0u32;
                if CM_Get_Parent(&mut parent, node, 0) != CR_SUCCESS {
                    break;
                }
                node = parent;
            }
            out.insert(number, chain);
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}
