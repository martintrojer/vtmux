//! Bringing a snapshot of an older format version up to
//! [`crate::FORMAT_VERSION`] (decision 6: newer reads older).
//!
//! A version bump that changes what an existing tag means does two things:
//! the record's decoder reads [`crate::wire::Ctx::version`] where the old
//! bytes must be read differently, and a step here fixes up what decoding
//! alone can't (a value that moved between records, a default that
//! changed). `STEPS[n]` takes a snapshot from version `MIN_VERSION + n` to
//! the next one.

use crate::{Error, FORMAT_VERSION, MIN_VERSION, Snapshot};

type Step = fn(&mut Snapshot) -> Result<(), Error>;

/// One step per version after [`MIN_VERSION`]; none yet.
const STEPS: [Step; (FORMAT_VERSION - MIN_VERSION) as usize] = [];

pub(crate) fn migrate(s: &mut Snapshot, from: u32) -> Result<(), Error> {
    run(s, from, MIN_VERSION, &STEPS)
}

fn run(s: &mut Snapshot, from: u32, min: u32, steps: &[Step]) -> Result<(), Error> {
    let skip = from.checked_sub(min).ok_or(Error::TooOld {
        found: from,
        oldest: min,
    })?;
    for step in steps.iter().skip(skip as usize) {
        step(s)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "a Step's signature")]
    fn to_2(s: &mut Snapshot) -> Result<(), Error> {
        s.meta.writer_version.push('2');
        Ok(())
    }

    #[expect(clippy::unnecessary_wraps, reason = "a Step's signature")]
    fn to_3(s: &mut Snapshot) -> Result<(), Error> {
        s.meta.writer_version.push('3');
        Ok(())
    }

    #[test]
    fn steps_run_in_order_from_the_stream_version() {
        let steps: [Step; 2] = [to_2, to_3];
        let mut s = Snapshot::default();
        run(&mut s, 1, 1, &steps).unwrap();
        assert_eq!(s.meta.writer_version, "23");
        let mut s = Snapshot::default();
        run(&mut s, 2, 1, &steps).unwrap();
        assert_eq!(s.meta.writer_version, "3");
        let mut s = Snapshot::default();
        run(&mut s, 3, 1, &steps).unwrap();
        assert_eq!(s.meta.writer_version, "");
        assert!(run(&mut s, 0, 1, &steps).is_err());
    }

    #[test]
    fn the_current_version_needs_no_step() {
        let mut s = Snapshot::default();
        migrate(&mut s, FORMAT_VERSION).unwrap();
        assert_eq!(s, Snapshot::default());
    }
}
