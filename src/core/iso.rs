//! Mounting and ejecting ISO images through the Virtual Disk API.
//! Windows 8+ lets standard users attach ISOs read-only, so no elevation is needed.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, OPEN_EXISTING, QueryDosDeviceW,
};
use windows::Win32::Storage::Vhd::{
    ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME, ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
    ATTACH_VIRTUAL_DISK_PARAMETERS, ATTACH_VIRTUAL_DISK_VERSION_1, AttachVirtualDisk,
    DETACH_VIRTUAL_DISK_FLAG_NONE, DetachVirtualDisk, GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
    GetStorageDependencyInformation, GetVirtualDiskPhysicalPath, OPEN_VIRTUAL_DISK_FLAG_NONE,
    OpenVirtualDisk, STORAGE_DEPENDENCY_INFO, STORAGE_DEPENDENCY_INFO_VERSION_2,
    VIRTUAL_DISK_ACCESS_DETACH, VIRTUAL_DISK_ACCESS_READ, VIRTUAL_STORAGE_TYPE,
    VIRTUAL_STORAGE_TYPE_DEVICE_ISO, VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::IOCTL_STORAGE_EJECT_MEDIA;
use windows::Win32::System::WindowsProgramming::DRIVE_CDROM;
use windows::Win32::UI::Shell::{ASSOCF_NONE, ASSOCSTR_PROGID, AssocQueryStringW};
use windows::core::{PCWSTR, PWSTR, w};

fn to_wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn is_iso(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("iso"))
}

/// True when Windows' built-in mounter is the default app for .iso files, in
/// which case opening an ISO should mount it, as Explorer does.
pub fn windows_mounts_iso_by_default() -> bool {
    let mut buf = [0u16; 128];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_PROGID,
            w!(".iso"),
            PCWSTR::null(),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .is_ok()
            && from_wide(&buf).eq_ignore_ascii_case("Windows.IsoFile")
    }
}

/// True for drives that can be ejected: mounted images and optical drives.
pub fn is_ejectable_drive(root: &Path) -> bool {
    let wide = to_wide(root);
    unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) == DRIVE_CDROM }
}

/// Mounts `iso` read-only until it is ejected (like Explorer) and returns its
/// drive root, e.g. `E:\`. An image that is already mounted returns its drive.
pub fn mount_iso(iso: &Path) -> Result<PathBuf, String> {
    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_ISO,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let wide_path = to_wide(iso);

    let device = unsafe {
        let mut handle = HANDLE::default();
        let err = OpenVirtualDisk(
            &storage_type,
            PCWSTR(wide_path.as_ptr()),
            VIRTUAL_DISK_ACCESS_READ,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            None,
            &mut handle,
        );
        if err != ERROR_SUCCESS {
            return Err(format!("could not open {}: {:?}", iso.display(), err));
        }

        let params = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: ATTACH_VIRTUAL_DISK_VERSION_1,
            ..Default::default()
        };
        let attach_err = AttachVirtualDisk(
            handle,
            None,
            ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY | ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME,
            0,
            Some(&params),
            None,
        );

        // Attaching an already-mounted image fails, but it still reports its device.
        let mut buf = [0u16; 260];
        let mut size = (buf.len() * 2) as u32;
        let path_err = GetVirtualDiskPhysicalPath(handle, &mut size, PWSTR(buf.as_mut_ptr()));
        let _ = CloseHandle(handle);

        if path_err != ERROR_SUCCESS {
            return Err(format!(
                "could not mount {}: {:?}",
                iso.display(),
                attach_err
            ));
        }
        from_wide(&buf)
    };

    // `\\.\CDROM0` -> `\Device\CdRom0`, the same target its drive letter points at.
    let device_name = device.trim_start_matches(r"\\.\");
    let target =
        dos_device_target(device_name).ok_or_else(|| format!("could not resolve {}", device))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(root) = drive_for_dos_target(&target) {
            return Ok(root);
        }
        if Instant::now() > deadline {
            return Err(format!("{} mounted but got no drive letter", iso.display()));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn dos_device_target(name: &str) -> Option<String> {
    let wide = to_wide(name);
    let mut buf = [0u16; 512];
    let len = unsafe { QueryDosDeviceW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    (len > 0).then(|| from_wide(&buf))
}

fn drive_for_dos_target(target: &str) -> Option<PathBuf> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| (mask >> i) & 1 == 1)
        .map(|i| (b'A' + i) as char)
        .find(|letter| {
            dos_device_target(&format!("{}:", letter))
                .is_some_and(|t| t.eq_ignore_ascii_case(target))
        })
        .map(|letter| PathBuf::from(format!("{}:\\", letter)))
}

/// Ejects the drive at `root`. A mounted image is detached entirely; a real
/// optical drive gets an eject request (opens the tray).
pub fn eject_drive(root: &Path) -> Result<(), String> {
    let letter = root
        .to_string_lossy()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .ok_or_else(|| format!("not a drive: {}", root.display()))?;
    let volume = to_wide(format!(r"\\.\{}:", letter));

    // Which image file (if any) backs this drive.
    let backing = unsafe {
        let handle = CreateFileW(
            PCWSTR(volume.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| format!("could not open {}: {}", root.display(), e))?;

        // u64 buffer keeps the struct's pointer fields aligned.
        let mut buffer = vec![0u64; 512];
        let info = buffer.as_mut_ptr() as *mut STORAGE_DEPENDENCY_INFO;
        (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2;
        let err = GetStorageDependencyInformation(
            handle,
            GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
            (buffer.len() * 8) as u32,
            info,
            None,
        );
        let _ = CloseHandle(handle);

        if err == ERROR_SUCCESS && (*info).NumberEntries > 0 {
            let entry = &(*info).Anonymous.Version2Entries[0];
            if entry.HostVolumeName.is_null() || entry.DependentVolumeRelativePath.is_null() {
                None
            } else {
                let image = entry.HostVolumeName.to_string().unwrap_or_default()
                    + &entry
                        .DependentVolumeRelativePath
                        .to_string()
                        .unwrap_or_default();
                Some((image, entry.VirtualStorageType))
            }
        } else {
            None
        }
    };

    unsafe {
        match backing {
            Some((image, storage_type)) => {
                let wide_image = to_wide(&image);
                let mut handle = HANDLE::default();
                let err = OpenVirtualDisk(
                    &storage_type,
                    PCWSTR(wide_image.as_ptr()),
                    VIRTUAL_DISK_ACCESS_DETACH,
                    OPEN_VIRTUAL_DISK_FLAG_NONE,
                    None,
                    &mut handle,
                );
                if err != ERROR_SUCCESS {
                    return Err(format!("could not open {}: {:?}", image, err));
                }
                let err = DetachVirtualDisk(handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0);
                let _ = CloseHandle(handle);
                if err != ERROR_SUCCESS {
                    return Err(format!("could not unmount {}: {:?}", image, err));
                }
                Ok(())
            }
            None => {
                let handle = CreateFileW(
                    PCWSTR(volume.as_ptr()),
                    GENERIC_READ.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
                .map_err(|e| format!("could not open {}: {}", root.display(), e))?;
                let result = DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_EJECT_MEDIA,
                    None,
                    0,
                    None,
                    0,
                    None,
                    None,
                );
                let _ = CloseHandle(handle);
                result.map_err(|e| format!("could not eject {}: {}", root.display(), e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_iso_extension_case_insensitively() {
        assert!(is_iso(Path::new(r"C:\images\disk.iso")));
        assert!(is_iso(Path::new(r"C:\images\DISK.ISO")));
        assert!(!is_iso(Path::new(r"C:\images\disk.img")));
        assert!(!is_iso(Path::new(r"C:\images\iso")));
    }
}
