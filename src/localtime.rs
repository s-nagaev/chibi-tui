//! Local wall-clock rendering of the per-message unix-seconds stamps.
//!
//! Every chat bubble carries the instant it was created; the transcript
//! shows it as the user's local `HH:MM` next to the author name. The
//! standard library offers no timezone conversion, so this module is the
//! single platform seam: POSIX `localtime_r` on unix and the C runtime's
//! `localtime_s` on Windows, both thread-safe, both reached through the
//! `libc` crate the workspace already links transitively.
//!
//! Degradation over panics: a stamp the platform cannot convert renders as
//! a fixed `--:--` placeholder. A skewed or ancient clock must never take
//! down the render loop.

/// Format a unix-seconds instant as the local `HH:MM`.
///
/// Never panics: an instant outside the platform's representable range
/// renders as `--:--`.
pub fn fmt_hhmm(unix_secs: u64) -> String {
    match local_hour_minute(unix_secs) {
        Some((hour, minute)) => format!("{hour:02}:{minute:02}"),
        None => "--:--".to_owned(),
    }
}

/// Local `(hour, minute)` of a unix-seconds instant, `None` when the
/// platform cannot convert it (unrepresentable stamp, conversion failure).
#[cfg(unix)]
fn local_hour_minute(unix_secs: u64) -> Option<(i32, i32)> {
    let secs: libc::time_t = unix_secs.try_into().ok()?;
    let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` writes into the caller-provided buffer and only
    // returns a null pointer on failure; the buffer outlives the call.
    let filled = unsafe { libc::localtime_r(&secs, broken_down.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: a non-null return means `localtime_r` initialized the buffer.
    let broken_down = unsafe { broken_down.assume_init() };
    Some((broken_down.tm_hour, broken_down.tm_min))
}

/// Windows counterpart: `localtime_s` takes the destination first (the
/// reverse of `localtime_r`) and reports failure through its return code.
#[cfg(windows)]
fn local_hour_minute(unix_secs: u64) -> Option<(i32, i32)> {
    let secs: libc::time_t = unix_secs.try_into().ok()?;
    let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_s` writes into the caller-provided buffer and
    // returns 0 on success; the buffer outlives the call.
    let failure = unsafe { libc::localtime_s(broken_down.as_mut_ptr(), &secs) };
    if failure != 0 {
        return None;
    }
    // SAFETY: a zero return means the buffer was initialized.
    let broken_down = unsafe { broken_down.assume_init() };
    Some((broken_down.tm_hour, broken_down.tm_min))
}

/// Targets without a C runtime timezone conversion (none of the release
/// targets, but the crate must still compile): no stamp is rendered.
#[cfg(not(any(unix, windows)))]
fn local_hour_minute(_unix_secs: u64) -> Option<(i32, i32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant (2023-11-14T22:13:20Z) — far from every DST switch of
    /// that autumn, so the local wall clock is stable across timezones.
    const STAMP: u64 = 1_700_000_000;

    /// The rendered shape is a zero-padded wall clock, independent of the
    /// machine's timezone.
    #[test]
    fn fmt_hhmm_renders_a_zero_padded_wall_clock() {
        let rendered = fmt_hhmm(STAMP);
        assert_eq!(rendered.len(), 5, "HH:MM is exactly five bytes: {rendered}");
        let (hour, minute) = rendered.split_at(2);
        assert_eq!(minute.chars().next(), Some(':'), "separator: {rendered}");
        assert!(
            hour.parse::<u32>().is_ok_and(|h| h < 24),
            "hour in range: {rendered}"
        );
        assert!(
            minute[1..].parse::<u32>().is_ok_and(|m| m < 60),
            "minute in range: {rendered}"
        );
        assert!(
            rendered.chars().all(|c| c.is_ascii_digit() || c == ':'),
            "digits and the separator only: {rendered}"
        );
    }

    /// Wall-clock semantics: one more hour moves the rendered hour by one
    /// and leaves the minute alone.
    #[test]
    fn fmt_hhmm_advances_one_hour_per_hour() {
        let before = fmt_hhmm(STAMP);
        let after = fmt_hhmm(STAMP + 3_600);
        let hour_before: u32 = before[..2].parse().expect("hour");
        let hour_after: u32 = after[..2].parse().expect("hour");
        assert_eq!(
            hour_after,
            (hour_before + 1) % 24,
            "one hour later: {before} → {after}"
        );
        assert_eq!(before[2..], after[2..], "minutes are untouched: {after}");
    }

    /// Whole days since the epoch do not move the wall clock: the render is
    /// a time of day, not an elapsed count.
    #[test]
    fn fmt_hhmm_ignores_whole_days() {
        assert_eq!(fmt_hhmm(STAMP), fmt_hhmm(STAMP + 86_400));
    }

    /// Out-of-range instants degrade to the placeholder instead of panicking.
    #[test]
    fn fmt_hhmm_degrades_for_unrepresentable_stamps() {
        assert_eq!(fmt_hhmm(u64::MAX), "--:--");
    }
}
