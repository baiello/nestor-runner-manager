//! Thin wrappers over the operating system: process identity, the peer of a Unix socket, code
//! signing, keys and the Keychain. macOS through `libproc`, `sysctl` and Security.framework;
//! other systems get the process functions only (the service targets macOS first).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// The identity of a process that survives pid reuse: its pid and the time it started, in
/// microseconds since the epoch, as the kernel recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_us: u64,
}

impl ProcessIdentity {
    pub fn of(pid: u32) -> Option<Self> {
        process_start_time_us(pid).map(|start_time_us| ProcessIdentity { pid, start_time_us })
    }

    /// Whether the process behind the pid is still this one (not a newer process given the
    /// same pid).
    pub fn is_current(&self) -> bool {
        process_start_time_us(self.pid) == Some(self.start_time_us)
    }
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (read == size).then_some(info)
}

#[cfg(target_os = "macos")]
pub fn process_start_time_us(pid: u32) -> Option<u64> {
    bsd_info(pid).map(|info| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "macos")]
pub fn parent_pid(pid: u32) -> Option<u32> {
    bsd_info(pid).map(|info| info.pbi_ppid)
}

/// The arguments a process was started with (`KERN_PROCARGS2`). Only readable for processes of
/// the same user.
#[cfg(target_os = "macos")]
pub fn process_arguments(pid: u32) -> Option<Vec<String>> {
    procargs(pid).map(|(arguments, _)| arguments)
}

/// The environment a process was started with (`KERN_PROCARGS2`, after the arguments). Only
/// readable for processes of the same user, and empty on recent macOS versions, which no longer
/// expose another process's environment.
#[cfg(target_os = "macos")]
pub fn process_environment(pid: u32) -> Option<Vec<(String, String)>> {
    procargs(pid).map(|(_, environment)| environment)
}

/// The arguments and the environment of a process.
#[cfg(target_os = "macos")]
type ProcArgs = (Vec<String>, Vec<(String, String)>);

#[cfg(target_os = "macos")]
fn procargs(pid: u32) -> Option<ProcArgs> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    if unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buffer.as_mut_ptr() as *mut libc::c_void, &mut size, std::ptr::null_mut(), 0)
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    if buffer.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?) as usize;
    let mut rest = &buffer[4..];
    // The executable path, then padding zeros, then argv, then the environment.
    let end = rest.iter().position(|b| *b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|b| *b != 0)?;
    rest = &rest[start..];
    let mut parts = rest.split(|b| *b == 0);
    let arguments: Vec<String> =
        parts.by_ref().take(argc).map(|part| String::from_utf8_lossy(part).into_owned()).collect();
    if arguments.len() != argc {
        return None;
    }
    let environment = parts
        .take_while(|part| !part.is_empty())
        .filter_map(|part| {
            let text = String::from_utf8_lossy(part);
            text.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    Some((arguments, environment))
}

/// What the sweep of a runner's processes needs to know about a process (KCA-734 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub uid: u32,
    pub start_time_us: u64,
    /// Ended, waiting for its parent to collect it: nothing left to stop.
    pub zombie: bool,
}

impl ProcessInfo {
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity { pid: self.pid, start_time_us: self.start_time_us }
    }
}

#[cfg(target_os = "macos")]
pub fn process_info(pid: u32) -> Option<ProcessInfo> {
    bsd_info(pid).map(|info| ProcessInfo {
        pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        uid: info.pbi_uid,
        start_time_us: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        zombie: info.pbi_status == libc::SZOMB,
    })
}

/// Every process id on the system.
#[cfg(target_os = "macos")]
pub fn all_pids() -> Vec<u32> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started meanwhile.
    let mut pids = vec![0 as libc::c_int; count as usize + 64];
    let size = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, size) };
    pids.truncate(filled.max(0) as usize);
    pids.into_iter().filter(|pid| *pid > 0).map(|pid| pid as u32).collect()
}

/// The paths of the files and directories a process of the same user has open.
#[cfg(target_os = "macos")]
pub fn process_open_paths(pid: u32) -> Vec<std::path::PathBuf> {
    // `struct vnode_fdinfowithpath` of <sys/proc_info.h>, not in the libc crate.
    #[repr(C)]
    struct FileInfo {
        open_flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guard_flags: u32,
    }
    #[repr(C)]
    struct VnodeWithPath {
        file: FileInfo,
        path: libc::vnode_info_path,
    }
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

    if pid == 0 || pid > i32::MAX as u32 {
        return Vec::new();
    }
    let item = std::mem::size_of::<libc::proc_fdinfo>();
    let size = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(size as usize / item + 16);
    let capacity = (fds.capacity() * item) as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut libc::c_void,
            capacity,
        )
    };
    if filled <= 0 {
        return Vec::new();
    }
    unsafe { fds.set_len(filled as usize / item) };
    fds.iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(|fd| {
            let mut info: VnodeWithPath = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<VnodeWithPath>() as libc::c_int;
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut _ as *mut libc::c_void,
                    size,
                )
            };
            (read == size).then(|| path_of(&info.path))?
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn path_of(info: &libc::vnode_info_path) -> Option<std::path::PathBuf> {
    let raw: &[libc::c_char] =
        unsafe { std::slice::from_raw_parts(info.vip_path.as_ptr() as *const libc::c_char, 32 * 32) };
    let bytes: Vec<u8> = raw.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    (!bytes.is_empty()).then(|| std::path::PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The current directory of a process of the same user.
#[cfg(target_os = "macos")]
pub fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if read != size {
        return None;
    }
    path_of(&info.pvi_cdir)
}

#[cfg(target_os = "linux")]
pub fn process_start_time_us(pid: u32) -> Option<u64> {
    // Start time in clock ticks after boot: enough to tell two processes with one pid apart.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 2..];
    after.split_whitespace().nth(19)?.parse::<u64>().ok()
}

#[cfg(target_os = "linux")]
pub fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 2..];
    after.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
pub fn process_arguments(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(raw.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
}

#[cfg(target_os = "linux")]
pub fn process_environment(pid: u32) -> Option<Vec<(String, String)>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter_map(|p| String::from_utf8_lossy(p).split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect(),
    )
}

#[cfg(target_os = "linux")]
pub fn process_info(pid: u32) -> Option<ProcessInfo> {
    use std::os::unix::fs::MetadataExt;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
    let uid = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    Some(ProcessInfo {
        pid,
        ppid: after.get(1)?.parse().ok()?,
        pgid: after.get(2)?.parse().ok()?,
        uid,
        start_time_us: after.get(19)?.parse().ok()?,
        zombie: *after.first()? == "Z",
    })
}

#[cfg(target_os = "linux")]
pub fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
pub fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "linux")]
pub fn process_open_paths(pid: u32) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|entries| entries.flatten().filter_map(|e| std::fs::read_link(e.path()).ok()).collect())
        .unwrap_or_default()
}

/// Whether `pid` is `ancestor` or one of its descendants, by walking up the parents. A process
/// that double-forked away is reparented to `launchd` and escapes this check: that is why the
/// local API does not rely on it alone (A4).
pub fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        match parent_pid(pid) {
            Some(parent) if parent != pid && parent > 1 => pid = parent,
            _ => return false,
        }
    }
    false
}

/// The user id of the process at the other end of a Unix socket, from the kernel.
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (result == 0).then_some(uid)
}

/// The kernel's audit token of the peer: identifies the process (pid and pid version), immune
/// to pid reuse, and is what code signing checks take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditToken(pub [u32; 8]);

impl AuditToken {
    pub fn pid(&self) -> u32 {
        self.0[5]
    }
}

#[cfg(target_os = "macos")]
pub fn peer_audit_token(stream: &UnixStream) -> Option<AuditToken> {
    let mut token = [0u32; 8];
    let mut size = std::mem::size_of_val(&token) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0, // SOL_LOCAL
            libc::LOCAL_PEERTOKEN,
            token.as_mut_ptr() as *mut libc::c_void,
            &mut size,
        )
    };
    (result == 0 && size as usize == std::mem::size_of_val(&token)).then_some(AuditToken(token))
}

#[cfg(not(target_os = "macos"))]
pub fn peer_audit_token(_stream: &UnixStream) -> Option<AuditToken> {
    None
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(target_os = "macos")]
pub use security::*;

/// Security.framework: code signing of a peer, P-256 keys, the data protection Keychain.
#[cfg(target_os = "macos")]
mod security {
    #![allow(non_upper_case_globals, non_camel_case_types)]

    use std::ffi::{c_char, c_void, CStr};
    use std::ptr;

    use super::AuditToken;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFErrorRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type SecKeyRef = *const c_void;
    type SecCodeRef = *const c_void;
    type SecRequirementRef = *const c_void;
    type SecAccessControlRef = *const c_void;
    type OSStatus = i32;
    type CFIndex = isize;
    type Boolean = u8;

    #[repr(C)]
    struct CFDictionaryKeyCallBacks([usize; 6]);
    #[repr(C)]
    struct CFDictionaryValueCallBacks([usize; 5]);

    const kCFStringEncodingUTF8: u32 = 0x0800_0100;
    const kCFNumberSInt64Type: CFIndex = 4;
    const kSecCSDefaultFlags: u32 = 0;
    const kSecCSSigningInformation: u32 = 1 << 1;
    const kSecCSDynamicInformation: u32 = 1 << 3;
    const kSecCodeSignatureRuntime: i64 = 0x10000;
    const kSecAccessControlPrivateKeyUsage: usize = 1 << 30;
    const errSecItemNotFound: OSStatus = -25300;
    const errSecDuplicateItem: OSStatus = -25299;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
        static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
        static kCFBooleanTrue: CFTypeRef;
        static kCFBooleanFalse: CFTypeRef;
        fn CFRelease(value: CFTypeRef);
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            length: CFIndex,
            encoding: u32,
            external: Boolean,
        ) -> CFStringRef;
        fn CFStringGetCString(string: CFStringRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
        fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
        fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
        fn CFDataGetLength(data: CFDataRef) -> CFIndex;
        fn CFDictionaryCreate(
            alloc: CFAllocatorRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            count: CFIndex,
            key_callbacks: *const CFDictionaryKeyCallBacks,
            value_callbacks: *const CFDictionaryValueCallBacks,
        ) -> CFDictionaryRef;
        fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
        fn CFNumberCreate(alloc: CFAllocatorRef, kind: CFIndex, value: *const c_void) -> CFNumberRef;
        fn CFNumberGetValue(number: CFNumberRef, kind: CFIndex, value: *mut c_void) -> Boolean;
        fn CFErrorCopyDescription(error: CFErrorRef) -> CFStringRef;
        fn CFDictionaryGetCount(dictionary: CFDictionaryRef) -> CFIndex;
        fn CFDictionaryGetKeysAndValues(dictionary: CFDictionaryRef, keys: *mut CFTypeRef, values: *mut CFTypeRef);
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecGuestAttributeAudit: CFStringRef;
        static kSecCodeInfoFlags: CFStringRef;
        static kSecCodeInfoEntitlementsDict: CFStringRef;
        static kSecAttrKeyType: CFStringRef;
        static kSecAttrKeyTypeECSECPrimeRandom: CFStringRef;
        static kSecAttrKeySizeInBits: CFStringRef;
        static kSecAttrKeyClass: CFStringRef;
        static kSecAttrKeyClassPublic: CFStringRef;
        static kSecAttrKeyClassPrivate: CFStringRef;
        static kSecAttrTokenID: CFStringRef;
        static kSecAttrTokenIDSecureEnclave: CFStringRef;
        static kSecPrivateKeyAttrs: CFStringRef;
        static kSecAttrIsPermanent: CFStringRef;
        static kSecAttrApplicationTag: CFStringRef;
        static kSecAttrAccessControl: CFStringRef;
        static kSecAttrAccessibleWhenUnlockedThisDeviceOnly: CFStringRef;
        static kSecAttrAccessible: CFStringRef;
        static kSecUseDataProtectionKeychain: CFStringRef;
        static kSecAttrAccessGroup: CFStringRef;
        static kSecClass: CFStringRef;
        static kSecClassKey: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrService: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecReturnRef: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecKeyAlgorithmECDSASignatureMessageX962SHA256: CFStringRef;

        fn SecCodeCopyGuestWithAttributes(
            host: SecCodeRef,
            attributes: CFDictionaryRef,
            flags: u32,
            guest: *mut SecCodeRef,
        ) -> OSStatus;
        fn SecRequirementCreateWithString(
            text: CFStringRef,
            flags: u32,
            requirement: *mut SecRequirementRef,
        ) -> OSStatus;
        fn SecCodeCheckValidity(code: SecCodeRef, flags: u32, requirement: SecRequirementRef) -> OSStatus;
        fn SecCodeCopySigningInformation(code: SecCodeRef, flags: u32, information: *mut CFDictionaryRef) -> OSStatus;

        fn SecKeyCreateRandomKey(parameters: CFDictionaryRef, error: *mut CFErrorRef) -> SecKeyRef;
        fn SecKeyCopyPublicKey(key: SecKeyRef) -> SecKeyRef;
        fn SecKeyCopyExternalRepresentation(key: SecKeyRef, error: *mut CFErrorRef) -> CFDataRef;
        fn SecKeyCreateWithData(data: CFDataRef, attributes: CFDictionaryRef, error: *mut CFErrorRef) -> SecKeyRef;
        fn SecKeyCreateSignature(
            key: SecKeyRef,
            algorithm: CFStringRef,
            data: CFDataRef,
            error: *mut CFErrorRef,
        ) -> CFDataRef;
        fn SecKeyVerifySignature(
            key: SecKeyRef,
            algorithm: CFStringRef,
            data: CFDataRef,
            signature: CFDataRef,
            error: *mut CFErrorRef,
        ) -> Boolean;
        fn SecAccessControlCreateWithFlags(
            alloc: CFAllocatorRef,
            protection: CFTypeRef,
            flags: usize,
            error: *mut CFErrorRef,
        ) -> SecAccessControlRef;
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
    }

    /// An owned Core Foundation object, released on drop.
    struct Owned(CFTypeRef);

    impl Owned {
        fn new(value: CFTypeRef) -> Option<Self> {
            (!value.is_null()).then(|| Owned(value))
        }
        fn get(&self) -> CFTypeRef {
            self.0
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }

    // SecKeyRef is thread-safe; the key handle is only used through Security.framework calls.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    fn string(text: &str) -> Owned {
        Owned::new(unsafe {
            CFStringCreateWithBytes(ptr::null(), text.as_ptr(), text.len() as CFIndex, kCFStringEncodingUTF8, 0)
        })
        .expect("CFString")
    }

    fn data(bytes: &[u8]) -> Owned {
        Owned::new(unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex) }).expect("CFData")
    }

    fn bytes_of(data: CFDataRef) -> Vec<u8> {
        unsafe {
            let length = CFDataGetLength(data) as usize;
            let pointer = CFDataGetBytePtr(data);
            if pointer.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(pointer, length).to_vec()
            }
        }
    }

    fn dictionary(pairs: &[(CFTypeRef, CFTypeRef)]) -> Owned {
        let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
        Owned::new(unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as CFIndex,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        })
        .expect("CFDictionary")
    }

    fn error_text(error: CFErrorRef) -> String {
        if error.is_null() {
            return "unknown error".into();
        }
        let error = Owned(error);
        let Some(description) = Owned::new(unsafe { CFErrorCopyDescription(error.get()) }) else {
            return "unknown error".into();
        };
        let mut buffer = [0 as c_char; 512];
        unsafe {
            if CFStringGetCString(
                description.get(),
                buffer.as_mut_ptr(),
                buffer.len() as CFIndex,
                kCFStringEncodingUTF8,
            ) != 0
            {
                CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
            } else {
                "unknown error".into()
            }
        }
    }

    // MARK: Code signing of a peer

    /// What code signing says about a peer process.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PeerSignature {
        /// Its signature is valid and satisfies the requirement.
        pub satisfies: bool,
        /// Signed with the hardened runtime (no library injection, no debugger attach).
        pub hardened_runtime: bool,
        /// Carries `com.apple.security.get-task-allow`: another process may take its task port.
        pub get_task_allow: bool,
        /// Carries a hardened runtime exception (`com.apple.security.cs.*`, such as
        /// `disable-library-validation` or `allow-dyld-environment-variables`): code of the same
        /// user could be loaded into it (F1).
        pub runtime_exceptions: bool,
    }

    /// The text of a CFString, if `value` is one.
    fn text_of(value: CFTypeRef) -> Option<String> {
        if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
            return None;
        }
        let mut buffer = [0 as c_char; 256];
        let ok =
            unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as CFIndex, kCFStringEncodingUTF8) };
        (ok != 0).then(|| unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned())
    }

    /// The keys of an entitlements dictionary whose value is not `false`.
    fn granted_entitlements(entitlements: CFDictionaryRef) -> Vec<String> {
        let count = unsafe { CFDictionaryGetCount(entitlements) }.max(0) as usize;
        let mut keys = vec![ptr::null(); count];
        let mut values = vec![ptr::null(); count];
        unsafe { CFDictionaryGetKeysAndValues(entitlements, keys.as_mut_ptr(), values.as_mut_ptr()) };
        keys.iter()
            .zip(&values)
            .filter(|(_, value)| **value != unsafe { kCFBooleanFalse })
            .filter_map(|(key, _)| text_of(*key))
            .collect()
    }

    /// Checks the process behind `token` against the code signing `requirement` (for example
    /// `anchor apple generic and identifier "fr.kocoa.nestor.runner-manager" and certificate
    /// leaf[subject.OU] = "TEAMID"`). The audit token, not the pid, names the process, so a pid
    /// reused meanwhile cannot pass for it.
    pub fn check_peer_signature(token: &AuditToken, requirement: &str) -> Result<PeerSignature, String> {
        let token_bytes: Vec<u8> = token.0.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let token_data = data(&token_bytes);
        let attributes = dictionary(&[(unsafe { kSecGuestAttributeAudit }, token_data.get())]);
        let mut code: SecCodeRef = ptr::null();
        let status =
            unsafe { SecCodeCopyGuestWithAttributes(ptr::null(), attributes.get(), kSecCSDefaultFlags, &mut code) };
        if status != 0 {
            return Err(format!("cannot identify the peer's code (status {status})"));
        }
        let code = Owned::new(code).ok_or("no code object")?;
        let text = string(requirement);
        let mut compiled: SecRequirementRef = ptr::null();
        let status = unsafe { SecRequirementCreateWithString(text.get(), kSecCSDefaultFlags, &mut compiled) };
        if status != 0 {
            return Err(format!("invalid code signing requirement (status {status})"));
        }
        let compiled = Owned::new(compiled).ok_or("no requirement object")?;
        let satisfies = unsafe { SecCodeCheckValidity(code.get(), kSecCSDefaultFlags, compiled.get()) } == 0;

        let mut information: CFDictionaryRef = ptr::null();
        let status = unsafe {
            SecCodeCopySigningInformation(
                code.get(),
                kSecCSSigningInformation | kSecCSDynamicInformation,
                &mut information,
            )
        };
        let (mut hardened_runtime, mut get_task_allow, mut runtime_exceptions) = (false, false, false);
        if status == 0 {
            if let Some(information) = Owned::new(information) {
                let flags = unsafe { CFDictionaryGetValue(information.get(), kSecCodeInfoFlags) };
                if !flags.is_null() {
                    let mut value: i64 = 0;
                    unsafe { CFNumberGetValue(flags, kCFNumberSInt64Type, &mut value as *mut i64 as *mut c_void) };
                    hardened_runtime = value & kSecCodeSignatureRuntime != 0;
                }
                let entitlements = unsafe { CFDictionaryGetValue(information.get(), kSecCodeInfoEntitlementsDict) };
                if !entitlements.is_null() {
                    let key = string("com.apple.security.get-task-allow");
                    let value = unsafe { CFDictionaryGetValue(entitlements, key.get()) };
                    get_task_allow = !value.is_null() && value == unsafe { kCFBooleanTrue };
                    runtime_exceptions = granted_entitlements(entitlements)
                        .iter()
                        .any(|name| name.starts_with("com.apple.security.cs."));
                }
            }
        }
        Ok(PeerSignature { satisfies, hardened_runtime, get_task_allow, runtime_exceptions })
    }

    // MARK: Keys

    /// A P-256 private key held by Security.framework. The private part is never returned by
    /// this type: it has no export method.
    pub struct SecKey {
        key: Owned,
    }

    impl SecKey {
        /// A key that lives in memory only (tests, the fake server, the demo).
        pub fn ephemeral() -> Result<SecKey, String> {
            let bits: i64 = 256;
            let size = Owned::new(unsafe {
                CFNumberCreate(ptr::null(), kCFNumberSInt64Type, &bits as *const i64 as *const c_void)
            })
            .ok_or("CFNumber")?;
            let parameters = dictionary(&[
                (unsafe { kSecAttrKeyType }, unsafe { kSecAttrKeyTypeECSECPrimeRandom }),
                (unsafe { kSecAttrKeySizeInBits }, size.get()),
            ]);
            Self::create(&parameters)
        }

        /// A key generated inside the Secure Enclave and kept in the data protection Keychain
        /// under `tag`. Its private part cannot leave the Secure Enclave. Needs a signed build
        /// with a `keychain-access-groups` entitlement; an unsigned build gets an error
        /// (`errSecMissingEntitlement`), which the service reports instead of falling back.
        pub fn secure_enclave(tag: &str, access_group: Option<&str>) -> Result<SecKey, String> {
            let mut error: CFErrorRef = ptr::null();
            let access = Owned::new(unsafe {
                SecAccessControlCreateWithFlags(
                    ptr::null(),
                    kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                    kSecAccessControlPrivateKeyUsage,
                    &mut error,
                )
            })
            .ok_or_else(|| error_text(error))?;
            let tag_data = data(tag.as_bytes());
            let group = access_group.map(string);
            let mut private: Vec<(CFTypeRef, CFTypeRef)> = vec![
                (unsafe { kSecAttrIsPermanent }, unsafe { kCFBooleanTrue }),
                (unsafe { kSecAttrApplicationTag }, tag_data.get()),
                (unsafe { kSecAttrAccessControl }, access.get()),
            ];
            if let Some(group) = &group {
                private.push((unsafe { kSecAttrAccessGroup }, group.get()));
            }
            let private = dictionary(&private);
            let bits: i64 = 256;
            let size = Owned::new(unsafe {
                CFNumberCreate(ptr::null(), kCFNumberSInt64Type, &bits as *const i64 as *const c_void)
            })
            .ok_or("CFNumber")?;
            let parameters = dictionary(&[
                (unsafe { kSecAttrKeyType }, unsafe { kSecAttrKeyTypeECSECPrimeRandom }),
                (unsafe { kSecAttrKeySizeInBits }, size.get()),
                (unsafe { kSecAttrTokenID }, unsafe { kSecAttrTokenIDSecureEnclave }),
                (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
                (unsafe { kSecPrivateKeyAttrs }, private.get()),
            ]);
            Self::create(&parameters)
        }

        /// Finds the Secure Enclave key stored under `tag`.
        pub fn find(tag: &str) -> Result<Option<SecKey>, String> {
            let tag_data = data(tag.as_bytes());
            let query = dictionary(&[
                (unsafe { kSecClass }, unsafe { kSecClassKey }),
                (unsafe { kSecAttrApplicationTag }, tag_data.get()),
                (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
                (unsafe { kSecReturnRef }, unsafe { kCFBooleanTrue }),
            ]);
            let mut result: CFTypeRef = ptr::null();
            match unsafe { SecItemCopyMatching(query.get(), &mut result) } {
                0 => Ok(Owned::new(result).map(|key| SecKey { key })),
                errSecItemNotFound => Ok(None),
                status => Err(format!("Keychain error {status}")),
            }
        }

        /// Deletes the Secure Enclave key stored under `tag`. Deleting a missing key succeeds.
        pub fn delete(tag: &str) -> Result<(), String> {
            let tag_data = data(tag.as_bytes());
            let query = dictionary(&[
                (unsafe { kSecClass }, unsafe { kSecClassKey }),
                (unsafe { kSecAttrApplicationTag }, tag_data.get()),
                (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
            ]);
            match unsafe { SecItemDelete(query.get()) } {
                0 | errSecItemNotFound => Ok(()),
                status => Err(format!("Keychain error {status}")),
            }
        }

        fn create(parameters: &Owned) -> Result<SecKey, String> {
            let mut error: CFErrorRef = ptr::null();
            let key = unsafe { SecKeyCreateRandomKey(parameters.get(), &mut error) };
            Owned::new(key).map(|key| SecKey { key }).ok_or_else(|| error_text(error))
        }

        /// A software key from its private X9.63 representation. Only the software fallback of
        /// A6 uses this, and only when it was explicitly allowed.
        pub fn import_private(bytes: &[u8]) -> Result<SecKey, String> {
            let key_data = data(bytes);
            let attributes = dictionary(&[
                (unsafe { kSecAttrKeyType }, unsafe { kSecAttrKeyTypeECSECPrimeRandom }),
                (unsafe { kSecAttrKeyClass }, unsafe { kSecAttrKeyClassPrivate }),
            ]);
            let mut error: CFErrorRef = ptr::null();
            Owned::new(unsafe { SecKeyCreateWithData(key_data.get(), attributes.get(), &mut error) })
                .map(|key| SecKey { key })
                .ok_or_else(|| error_text(error))
        }

        /// The private part of a *software* key. A Secure Enclave key refuses. Only the software
        /// fallback of A6 uses this, to keep its key in the Keychain.
        pub fn export_private_software(&self) -> Result<Vec<u8>, String> {
            let mut error: CFErrorRef = ptr::null();
            let bytes = Owned::new(unsafe { SecKeyCopyExternalRepresentation(self.key.get(), &mut error) })
                .ok_or_else(|| error_text(error))?;
            Ok(bytes_of(bytes.get()))
        }

        /// The public key, X9.63 uncompressed (65 bytes).
        pub fn public_key(&self) -> Result<Vec<u8>, String> {
            let public = Owned::new(unsafe { SecKeyCopyPublicKey(self.key.get()) }).ok_or("no public key")?;
            let mut error: CFErrorRef = ptr::null();
            let bytes = Owned::new(unsafe { SecKeyCopyExternalRepresentation(public.get(), &mut error) })
                .ok_or_else(|| error_text(error))?;
            Ok(bytes_of(bytes.get()))
        }

        /// ECDSA P-256 over SHA-256, X9.62 DER signature.
        pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
            let message = data(message);
            let mut error: CFErrorRef = ptr::null();
            let signature = Owned::new(unsafe {
                SecKeyCreateSignature(
                    self.key.get(),
                    kSecKeyAlgorithmECDSASignatureMessageX962SHA256,
                    message.get(),
                    &mut error,
                )
            })
            .ok_or_else(|| error_text(error))?;
            Ok(bytes_of(signature.get()))
        }
    }

    /// Verifies a signature made by [`SecKey::sign`] with the X9.63 `public_key`.
    pub fn verify_signature(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
        let key_data = data(public_key);
        let attributes = dictionary(&[
            (unsafe { kSecAttrKeyType }, unsafe { kSecAttrKeyTypeECSECPrimeRandom }),
            (unsafe { kSecAttrKeyClass }, unsafe { kSecAttrKeyClassPublic }),
        ]);
        let mut error: CFErrorRef = ptr::null();
        let Some(key) = Owned::new(unsafe { SecKeyCreateWithData(key_data.get(), attributes.get(), &mut error) })
        else {
            if !error.is_null() {
                unsafe { CFRelease(error) };
            }
            return false;
        };
        let (message, signature) = (data(message), data(signature));
        let mut error: CFErrorRef = ptr::null();
        let valid = unsafe {
            SecKeyVerifySignature(
                key.get(),
                kSecKeyAlgorithmECDSASignatureMessageX962SHA256,
                message.get(),
                signature.get(),
                &mut error,
            )
        } != 0;
        if !error.is_null() {
            unsafe { CFRelease(error) };
        }
        valid
    }

    // MARK: Data protection Keychain

    /// Stores a secret in the data protection Keychain. Access to such items is bound to the
    /// code signature (team and access group), not to a path: an unsigned build cannot use it.
    pub fn keychain_put(service: &str, account: &str, secret: &[u8], access_group: Option<&str>) -> Result<(), String> {
        keychain_delete(service, account, access_group)?;
        let (service, account, value) = (string(service), string(account), data(secret));
        let group = access_group.map(string);
        let mut attributes: Vec<(CFTypeRef, CFTypeRef)> = vec![
            (unsafe { kSecClass }, unsafe { kSecClassGenericPassword }),
            (unsafe { kSecAttrService }, service.get()),
            (unsafe { kSecAttrAccount }, account.get()),
            (unsafe { kSecValueData }, value.get()),
            (unsafe { kSecAttrAccessible }, unsafe { kSecAttrAccessibleWhenUnlockedThisDeviceOnly }),
            (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
        ];
        if let Some(group) = &group {
            attributes.push((unsafe { kSecAttrAccessGroup }, group.get()));
        }
        let attributes = dictionary(&attributes);
        match unsafe { SecItemAdd(attributes.get(), ptr::null_mut()) } {
            0 => Ok(()),
            errSecDuplicateItem => Err("Keychain item already exists".into()),
            status => Err(format!("Keychain error {status}")),
        }
    }

    pub fn keychain_get(service: &str, account: &str, access_group: Option<&str>) -> Result<Option<Vec<u8>>, String> {
        let (service, account) = (string(service), string(account));
        let group = access_group.map(string);
        let mut query: Vec<(CFTypeRef, CFTypeRef)> = vec![
            (unsafe { kSecClass }, unsafe { kSecClassGenericPassword }),
            (unsafe { kSecAttrService }, service.get()),
            (unsafe { kSecAttrAccount }, account.get()),
            (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
            (unsafe { kSecReturnData }, unsafe { kCFBooleanTrue }),
        ];
        if let Some(group) = &group {
            query.push((unsafe { kSecAttrAccessGroup }, group.get()));
        }
        let query = dictionary(&query);
        let mut result: CFTypeRef = ptr::null();
        match unsafe { SecItemCopyMatching(query.get(), &mut result) } {
            0 => Ok(Owned::new(result).map(|d| bytes_of(d.get()))),
            errSecItemNotFound => Ok(None),
            status => Err(format!("Keychain error {status}")),
        }
    }

    pub fn keychain_delete(service: &str, account: &str, access_group: Option<&str>) -> Result<(), String> {
        let (service, account) = (string(service), string(account));
        let group = access_group.map(string);
        let mut query: Vec<(CFTypeRef, CFTypeRef)> = vec![
            (unsafe { kSecClass }, unsafe { kSecClassGenericPassword }),
            (unsafe { kSecAttrService }, service.get()),
            (unsafe { kSecAttrAccount }, account.get()),
            (unsafe { kSecUseDataProtectionKeychain }, unsafe { kCFBooleanTrue }),
        ];
        if let Some(group) = &group {
            query.push((unsafe { kSecAttrAccessGroup }, group.get()));
        }
        let query = dictionary(&query);
        match unsafe { SecItemDelete(query.get()) } {
            0 | errSecItemNotFound => Ok(()),
            status => Err(format!("Keychain error {status}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_tells_a_live_process_from_a_reused_pid() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let identity = ProcessIdentity::of(child.id()).unwrap();
        assert!(identity.is_current());
        let older = ProcessIdentity { start_time_us: identity.start_time_us - 1, ..identity };
        assert!(!older.is_current(), "same pid, another start time: not the same process");
        assert!(descends_from(child.id(), std::process::id()));
        assert_eq!(process_arguments(child.id()).unwrap(), vec!["/bin/sleep".to_string(), "5".to_string()]);
        let info = process_info(child.id()).unwrap();
        assert_eq!((info.ppid, info.uid, info.zombie), (std::process::id(), unsafe { libc::geteuid() }, false));
        assert!(all_pids().contains(&child.id()));
        assert_eq!(process_cwd(child.id()).unwrap(), std::env::current_dir().unwrap().canonicalize().unwrap());
        // Recent macOS versions no longer expose another process's environment.
        let environment = process_environment(child.id()).unwrap();
        if cfg!(target_os = "linux") {
            assert!(environment.contains(&("HOME".to_string(), std::env::var("HOME").unwrap())), "{environment:?}");
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!identity.is_current());
    }

    #[test]
    fn open_files_are_listed() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("out.log");
        let output = std::fs::File::create(&file).unwrap();
        let mut child =
            std::process::Command::new("/bin/sleep").arg("5").current_dir("/").stdout(output).spawn().unwrap();
        let canonical = file.canonicalize().unwrap();
        let paths = process_open_paths(child.id());
        assert!(paths.contains(&canonical), "{paths:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])).unwrap(), vec![0, 1, 254, 255]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ephemeral_keys_sign_and_verify() {
        let key = SecKey::ephemeral().unwrap();
        let public = key.public_key().unwrap();
        assert_eq!(public.len(), 65);
        let signature = key.sign(b"hello").unwrap();
        assert!(verify_signature(&public, b"hello", &signature));
        assert!(!verify_signature(&public, b"hellp", &signature));
        let other = SecKey::ephemeral().unwrap().public_key().unwrap();
        assert!(!verify_signature(&other, b"hello", &signature));
    }
}
