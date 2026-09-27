//! Owner-only access to the configuration directory, the configuration file and
//! the private keys it names.
//!
//! Unix states this as a file mode (`0600` for files, `0700` for directories).
//! Windows has no mode, so the same guarantee is stated as a DACL that grants
//! access only to the owner of the file, `SYSTEM` and `Administrators` — the
//! accounts that can read any file on the machine regardless of its ACL. Both
//! implementations read the same way: [`restrict`] applies the guarantee,
//! [`check`] reports why an existing file does not satisfy it.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    pub fn restrict(path: &Path, mode: u32) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    /// `mode` is the exact mode the file must carry, because on Unix every bit
    /// set beyond it is a grant to somebody else.
    pub fn check(path: &Path, mode: u32) -> Result<(), String> {
        let actual = fs::metadata(path)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode()
            & 0o777;
        if actual == mode {
            return Ok(());
        }
        Err(format!("expected mode {mode:o}, found {actual:o}"))
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetLengthSid, GetSecurityDescriptorDacl,
        GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Well-known SIDs that keep access. Both name accounts that can read any
    /// file on the machine, so excluding them would buy nothing.
    const LOCAL_SYSTEM: &str = "SY";
    const ADMINISTRATORS: &str = "BA";
    /// Only meaningful on an inheritable ACE, where it names whoever creates the
    /// child — this user.
    const CREATOR_OWNER: &str = "CO";

    /// A block the security APIs allocated, released with `LocalFree`.
    struct LocalBlock(*mut std::ffi::c_void);

    impl Drop for LocalBlock {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }

    /// A token handle, closed on drop. `GetCurrentProcess` returns a
    /// pseudo-handle that must never be closed, so it is only ever passed to
    /// `OpenProcessToken`, never wrapped here.
    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn to_wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The DACL that makes a path owner-only. A directory carries inherit-only
    /// ACEs so everything created inside it inherits the same guarantee.
    fn owner_only_sddl(inheritable: bool) -> String {
        let flags = if inheritable { "OICI" } else { "" };
        let owner = current_user_sid();
        format!(
            "D:P(A;{flags};FA;;;{owner})(A;{flags};FA;;;{LOCAL_SYSTEM})(A;{flags};FA;;;{ADMINISTRATORS})"
        )
    }

    pub fn restrict(path: &Path, _mode: u32) -> io::Result<()> {
        let sddl = to_wide(OsStr::new(&owner_only_sddl(path.is_dir())));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut size = 0u32;
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                &mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalBlock(descriptor);

        let mut present = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            return Err(io::Error::other(
                "the generated security descriptor carries no DACL",
            ));
        }

        let path = to_wide(path.as_os_str());
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    /// Reports a grant to any account that is not the owner, `SYSTEM` or
    /// `Administrators`. Deny and audit ACEs are skipped, because they take away
    /// rather than hand out access.
    pub fn check(path: &Path, _mode: u32) -> Result<(), String> {
        let dacl = dacl_sddl(path).map_err(|error| error.to_string())?;
        let owner = current_user_sid();
        let allowed = [owner.as_str(), LOCAL_SYSTEM, ADMINISTRATORS, CREATOR_OWNER];

        for ace in ace_fields(&dacl) {
            let kind = ace.first().copied().unwrap_or_default();
            match kind {
                "A" => {
                    let granted = ace.get(5).copied().unwrap_or_default();
                    if !allowed.contains(&granted) {
                        return Err(format!("{granted} is granted access"));
                    }
                }
                // An object ACE grants access to the same SIDs but keeps them
                // behind a GUID this check does not read, so it is refused rather
                // than waved through.
                "OA" => return Err("object ACE on a file that must stay owner-only".into()),
                _ => {}
            }
        }
        Ok(())
    }

    /// The DACL of `path` rendered as SDDL, for example
    /// `D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-...-1001)`.
    fn dacl_sddl(path: &Path) -> io::Result<String> {
        let path = to_wide(path.as_os_str());
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = LocalBlock(descriptor);
        // A null DACL is the dangerous case: it grants every account full
        // control, which no ACE enumeration would ever reveal.
        if dacl.is_null() {
            return Err(io::Error::other(
                "the file has no DACL, so every account has full control",
            ));
        }

        let mut text: *mut u16 = std::ptr::null_mut();
        let mut length = 0u32;
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let text = LocalBlock(text.cast());
        Ok(wide_to_string(text.0.cast::<u16>()))
    }

    /// Applies an arbitrary DACL, so tests can build the loosened file this
    /// module exists to detect.
    #[cfg(test)]
    pub fn apply_sddl(path: &Path, sddl: &str) -> io::Result<()> {
        let sddl = to_wide(OsStr::new(sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut size = 0u32;
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                &mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalBlock(descriptor);
        let mut present = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = 0;
        unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) };
        let path = to_wide(path.as_os_str());
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn wide_to_string(value: *const u16) -> String {
        let mut length = 0usize;
        while unsafe { *value.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) })
    }

    /// The six `;`-separated fields of every ACE in an SDDL DACL string.
    fn ace_fields(dacl: &str) -> Vec<Vec<&str>> {
        let mut fields = Vec::new();
        let mut rest = dacl;
        while let Some(open) = rest.find('(') {
            let after = &rest[open + 1..];
            let Some(close) = after.find(')') else { break };
            fields.push(after[..close].split(';').collect());
            rest = &after[close..];
        }
        fields
    }

    /// The SID string of the account running this process.
    fn current_user_sid() -> String {
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return String::new();
        }
        let token = Token(token);

        let mut needed = 0u32;
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return String::new();
        }
        let mut buffer = vec![0u8; needed as usize];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return String::new();
        }
        // `TOKEN_USER` points at a SID inside the same buffer, so the SID is
        // copied out before the buffer is dropped.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
        if length == 0 {
            return String::new();
        }
        let sid = unsafe { std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), length) };

        let mut text: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid.as_ptr() as *mut _, &mut text) } == 0 {
            return String::new();
        }
        let text = LocalBlock(text.cast());
        wide_to_string(text.0.cast::<u16>())
    }
}

#[cfg(unix)]
pub use unix::{check, restrict};

#[cfg(windows)]
pub use windows::{check, restrict};

#[cfg(all(windows, test))]
pub use windows::apply_sddl;
