use std::{fs::OpenOptions, io, path::Path};

#[cfg(unix)]
fn allowed_mode(original: Option<&Path>) -> io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;

    let mode = match original {
        Some(path) => std::fs::metadata(path)?.permissions().mode(),
        None => 0o600,
    };
    Ok(mode & 0o600)
}

pub(super) fn configure_create(
    options: &mut OpenOptions,
    original: Option<&Path>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(allowed_mode(original)?);
    }
    #[cfg(not(unix))]
    let _ = (options, original);
    Ok(())
}

#[cfg(unix)]
pub(super) fn restrict_copy(path: &Path, original: Option<&Path>) -> io::Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};

    let mode = fs::metadata(path)?.permissions().mode() & 0o7777;
    let restricted = mode & allowed_mode(original)?;
    if restricted != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(restricted))?;
    }
    Ok(())
}

#[cfg(windows)]
pub(super) use windows_acl::restrict_copy;

#[cfg(not(any(unix, windows)))]
pub(super) fn restrict_copy(_path: &Path, _original: Option<&Path>) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows_acl {
    use std::{ffi::c_void, io, os::windows::ffi::OsStrExt, path::Path, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
                SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
            PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        },
    };

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: the Win32 authorization APIs allocate this buffer for LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }

    struct Dacl {
        descriptor: LocalAllocation,
        entries: *const ACL,
        protected: bool,
    }

    impl Dacl {
        fn read(path: &Path) -> io::Result<Self> {
            let name = wide_path(path)?;
            let mut descriptor = ptr::null_mut();
            let mut entries = ptr::null_mut();
            // SAFETY: name is NUL terminated and both output pointers are writable.
            status(unsafe {
                GetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut entries,
                    ptr::null_mut(),
                    &mut descriptor,
                )
            })?;
            let descriptor = LocalAllocation(descriptor);
            let mut control = 0;
            let mut revision = 0;
            // SAFETY: the descriptor remains owned and the output pointers are writable.
            if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                descriptor,
                entries,
                protected: control & SE_DACL_PROTECTED != 0,
            })
        }

        fn sddl(&self) -> io::Result<Option<String>> {
            // A null DACL already grants full access and cannot be converted to SDDL.
            if self.entries.is_null() {
                return Ok(None);
            }
            let mut text = ptr::null_mut();
            let mut len = 0;
            // SAFETY: the descriptor stays live and the conversion owns the output buffer.
            if unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    self.descriptor.0,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    &mut len,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _text = LocalAllocation(text.cast());
            if text.is_null() || len == 0 {
                return Err(io::Error::other("没有读取到工单库的 DACL"));
            }
            // SAFETY: on success text points to the reported number of UTF-16 units.
            let units = unsafe { std::slice::from_raw_parts(text, len as usize) };
            Ok(Some(
                String::from_utf16_lossy(units)
                    .trim_end_matches('\0')
                    .to_owned(),
            ))
        }
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut name: Vec<_> = std::path::absolute(path)?
            .as_os_str()
            .encode_wide()
            .collect();
        if name.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        name.push(0);
        Ok(name)
    }

    fn status(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    fn write(path: &Path, entries: *const ACL, protection: Option<bool>) -> io::Result<()> {
        let name = wide_path(path)?;
        let information = DACL_SECURITY_INFORMATION
            | match protection {
                Some(true) => PROTECTED_DACL_SECURITY_INFORMATION,
                Some(false) => UNPROTECTED_DACL_SECURITY_INFORMATION,
                None => 0,
            };
        // SAFETY: name is NUL terminated and entries points to a live DACL (or is null).
        status(unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                information,
                ptr::null_mut(),
                ptr::null_mut(),
                entries,
                ptr::null(),
            )
        })
    }

    pub(crate) fn restrict_copy(path: &Path, original: Option<&Path>) -> io::Result<()> {
        let Some(original) = original else {
            return Ok(());
        };
        let original = Dacl::read(original)?;
        let expected = original.sddl()?;
        let copy = Dacl::read(path)?;
        if copy.protected == original.protected && copy.sddl()? == expected {
            return Ok(());
        }
        let protection = (copy.protected != original.protected).then_some(original.protected);
        write(path, original.entries, protection)?;
        let copy = Dacl::read(path)?;
        if copy.protected != original.protected || copy.sddl()? != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "无法保留原工单库的 DACL",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn read_sddl(path: &Path) -> io::Result<String> {
        Dacl::read(path)?
            .sddl()?
            .ok_or_else(|| io::Error::other("expected a non-null DACL"))
    }

    #[cfg(test)]
    pub(super) fn set_test_dacl(path: &Path) -> io::Result<()> {
        use windows_sys::Win32::Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityDescriptorDacl,
        };

        // Deny execution to Everyone. The owner retains full access to the database.
        let sddl: Vec<_> = "D:P(D;;0x20;;;WD)(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)"
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: sddl is NUL terminated and the output pointer is writable.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalAllocation(descriptor);
        let (mut present, mut entries, mut defaulted) = (0, ptr::null_mut(), 0);
        // SAFETY: the descriptor stays live and the output pointers are writable.
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut entries, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        write(path, entries, Some(true))
    }
}

#[cfg(all(test, windows))]
pub(super) fn read_sddl(path: &Path) -> io::Result<String> {
    windows_acl::read_sddl(path)
}

#[cfg(all(test, windows))]
pub(super) fn set_test_dacl(path: &Path) -> io::Result<()> {
    windows_acl::set_test_dacl(path)
}
