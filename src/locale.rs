use std::ffi::{CStr, CString};

/// Adopts the user's locale from the environment, as `setlocale(LC_ALL, "")`
/// does in the C implementation. Date formatting, the decimal separator and
/// how command output bytes are decoded all depend on it.
pub fn init() {
    // SAFETY: called once at startup before any other thread exists.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }
}

/// Whether the active locale encodes text as UTF-8. Any other encoding is
/// treated as single-byte ASCII, where bytes above 0x7f are invalid.
pub fn is_utf8() -> bool {
    // SAFETY: nl_langinfo returns a pointer to a static, NUL-terminated string.
    let codeset = unsafe {
        let p = libc::nl_langinfo(libc::CODESET);
        if p.is_null() {
            return false;
        }
        CStr::from_ptr(p).to_bytes().to_ascii_uppercase()
    };
    codeset == b"UTF-8" || codeset == b"UTF8"
}

fn decimal_point() -> String {
    // SAFETY: localeconv returns a pointer to static storage that stays valid
    // until the next setlocale call, and it is copied out immediately.
    unsafe {
        let lc = libc::localeconv();
        if lc.is_null() || (*lc).decimal_point.is_null() {
            return ".".to_string();
        }
        let dp = CStr::from_ptr((*lc).decimal_point).to_string_lossy();
        if dp.is_empty() {
            ".".to_string()
        } else {
            dp.into_owned()
        }
    }
}

/// Formats `value` like printf's `%.<precision>f`, using the locale's decimal
/// separator.
pub fn format_fixed(value: f64, precision: usize) -> String {
    let s = format!("{value:.precision$}");
    let dp = decimal_point();
    if dp == "." {
        s
    } else {
        s.replacen('.', &dp, 1)
    }
}

/// Formats the current local time with `strftime(3)`.
pub fn strftime_now(format: &str) -> String {
    let Ok(fmt) = CString::new(format) else {
        return String::new();
    };
    // SAFETY: `tm` is fully initialized by localtime_r before strftime reads
    // it, and strftime never writes past `buf.len()` bytes.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 256];
        let n = libc::strftime(
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            fmt.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn unix_time() -> libc::time_t {
    // SAFETY: time(NULL) has no preconditions.
    unsafe { libc::time(std::ptr::null_mut()) }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 257];
    // SAFETY: gethostname writes at most 256 bytes into the 257-byte buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), 256) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(256);
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn strerror(errno: i32) -> String {
    // SAFETY: strerror returns a pointer to a NUL-terminated string that is
    // copied before any other libc call can overwrite it.
    unsafe {
        let p = libc::strerror(errno);
        if p.is_null() {
            return format!("Unknown error {errno}");
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
