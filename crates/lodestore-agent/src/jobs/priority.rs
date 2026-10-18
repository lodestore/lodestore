//! CPU and disk priority for a backup job. The policy can only lower the
//! priority of the job thread, so a backup stays out of the way of the
//! workload on the host. The hooks and the other processes the job starts
//! afterwards inherit it.

use std::io;

use lodestore_proto::agent::v1::{IoniceClass, Limits};

const MAX_NICE: u32 = 19;
const MAX_IONICE_LEVEL: u32 = 7;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_BE: i32 = 2;
const IOPRIO_CLASS_IDLE: i32 = 3;

/// Value for `ioprio_set`: class in the high bits, level in the low ones.
/// `None` when the limits ask for no class.
fn ioprio_value(class: i32, level: u32) -> Option<i32> {
    let (class, level) = match IoniceClass::try_from(class) {
        Ok(IoniceClass::BestEffort) => (IOPRIO_CLASS_BE, level.min(MAX_IONICE_LEVEL) as i32),
        Ok(IoniceClass::Idle) => (IOPRIO_CLASS_IDLE, 0),
        _ => return None,
    };
    Some((class << IOPRIO_CLASS_SHIFT) | level)
}

fn apply_with(
    limits: &Limits,
    set_nice: impl FnOnce(i32) -> io::Result<()>,
    set_ioprio: impl FnOnce(i32) -> io::Result<()>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if limits.nice > 0 {
        let nice = limits.nice.min(MAX_NICE) as i32;
        if let Err(e) = set_nice(nice) {
            warnings.push(format!("cannot set the nice value to {nice}: {e}"));
        }
    }
    if let Some(value) = ioprio_value(limits.ionice_class, limits.ionice_level)
        && let Err(e) = set_ioprio(value)
    {
        warnings.push(format!("cannot set the ionice class: {e}"));
    }
    warnings
}

/// Lowers the priority of the calling thread as the limits ask and returns a
/// warning for every part that could not be applied. Does nothing outside
/// Linux.
pub fn apply(limits: &Limits) -> Vec<String> {
    apply_with(limits, platform::set_nice, platform::set_ioprio)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;

    const IOPRIO_WHO_PROCESS: libc::c_long = 1;

    pub fn set_nice(nice: i32) -> io::Result<()> {
        rustix::process::setpriority_process(None, nice).map_err(io::Error::from)
    }

    pub fn set_ioprio(value: i32) -> io::Result<()> {
        // SAFETY: `ioprio_set` takes three integers and touches no memory of
        // ours; `who = 0` addresses the calling thread.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0 as libc::c_long,
                libc::c_long::from(value),
            )
        };
        if rc == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;

    pub fn set_nice(_: i32) -> io::Result<()> {
        Ok(())
    }

    pub fn set_ioprio(_: i32) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn limits(nice: u32, class: IoniceClass, level: u32) -> Limits {
        Limits {
            nice,
            ionice_class: class as i32,
            ionice_level: level,
            ..Default::default()
        }
    }

    fn denied() -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    #[test]
    fn priority_ioprio_value() {
        assert_eq!(
            ioprio_value(IoniceClass::BestEffort as i32, 5),
            Some((2 << 13) | 5)
        );
        assert_eq!(
            ioprio_value(IoniceClass::BestEffort as i32, 99),
            Some((2 << 13) | 7)
        );
        assert_eq!(ioprio_value(IoniceClass::Idle as i32, 4), Some(3 << 13));
        assert_eq!(ioprio_value(IoniceClass::Unspecified as i32, 4), None);
        assert_eq!(ioprio_value(77, 0), None);
    }

    #[test]
    fn priority_default_calls_nothing() {
        let touched = Cell::new(false);
        let warnings = apply_with(
            &limits(0, IoniceClass::Unspecified, 3),
            |_| {
                touched.set(true);
                Ok(())
            },
            |_| {
                touched.set(true);
                Ok(())
            },
        );
        assert!(warnings.is_empty());
        assert!(!touched.get());
    }

    #[test]
    fn priority_never_raises() {
        let seen = Cell::new(0);
        let warnings = apply_with(
            &limits(500, IoniceClass::Idle, 0),
            |n| {
                seen.set(n);
                Ok(())
            },
            |v| {
                assert_eq!(v >> IOPRIO_CLASS_SHIFT, IOPRIO_CLASS_IDLE);
                Ok(())
            },
        );
        assert!(warnings.is_empty());
        assert_eq!(seen.get(), 19);
    }

    #[test]
    fn priority_failures_become_warnings() {
        let warnings = apply_with(
            &limits(10, IoniceClass::BestEffort, 2),
            |_| denied(),
            |_| denied(),
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
