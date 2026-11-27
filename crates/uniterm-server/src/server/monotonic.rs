//! Kernel monotonic timestamps retain their meaning across local engine images.
//! `Instant` has an opaque epoch, so translate at the checkpoint boundary.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::{Duration, Instant};

fn sample() -> std::io::Result<(Instant, Duration)> {
    let now = Instant::now();
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is writable and the kernel fills both fields on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((now, Duration::new(time.tv_sec as u64, time.tv_nsec as u32)))
}

fn encode(value: Instant) -> std::io::Result<Duration> {
    let (now, kernel) = sample()?;
    if value >= now {
        kernel.checked_add(value - now)
    } else {
        kernel.checked_sub(now - value)
    }
    .ok_or_else(|| std::io::Error::other("monotonic timestamp outside clock range"))
}

fn decode(value: Duration) -> std::io::Result<Instant> {
    let (now, kernel) = sample()?;
    if value >= kernel {
        now.checked_add(value - kernel)
    } else {
        now.checked_sub(kernel - value)
    }
    .ok_or_else(|| std::io::Error::other("monotonic timestamp outside clock range"))
}

pub(super) fn serialize<S: Serializer>(value: &Instant, out: S) -> Result<S::Ok, S::Error> {
    encode(*value)
        .map_err(serde::ser::Error::custom)?
        .serialize(out)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Instant, D::Error> {
    decode(Duration::deserialize(input)?).map_err(serde::de::Error::custom)
}

pub(super) mod option {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Instant>,
        out: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .map(encode)
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .serialize(out)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Option<Instant>, D::Error> {
        Option::<Duration>::deserialize(input)?
            .map(decode)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}
