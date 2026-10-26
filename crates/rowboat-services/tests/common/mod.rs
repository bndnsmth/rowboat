//! OS-neutral process high-water RSS, always expressed in KiB.
pub fn peak_kib() -> anyhow::Result<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the entire structure on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let usage = unsafe { usage.assume_init() };
    let peak = u64::try_from(usage.ru_maxrss)?;
    // Darwin reports bytes; Linux reports KiB.
    #[cfg(target_os = "macos")]
    let peak = peak / 1024;
    Ok(peak)
}
