//! Producer-owned input ownership stamps. A stamp belongs to a read batch, not
//! to an individual send: sending may block while the modal closes.
use std::sync::atomic::{AtomicU64, Ordering};

use super::ClientLoopEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputEpoch(pub(super) u64);

impl InputEpoch {
    pub(super) fn capture(epoch: &AtomicU64) -> Self {
        Self(epoch.load(Ordering::Acquire))
    }

    /// Ownership advances only across an observed no-data boundary without
    /// partial framing. Capture `candidate` BEFORE polling. Readable bytes and
    /// entire fragment-completion batches retain their existing owner, even if
    /// that conservatively drops fresh typeahead until the next idle boundary.
    pub(super) fn after_poll(self, candidate: Self, readable: bool, pending: bool) -> Self {
        if readable || pending {
            self
        } else {
            candidate
        }
    }

    pub(super) fn own(self, event: ClientLoopEvent) -> ClientLoopEvent {
        match event {
            #[cfg(unix)]
            ClientLoopEvent::StdinInput(_) | ClientLoopEvent::PixelMouse(..) => {
                ClientLoopEvent::OwnedInput {
                    epoch: self.0,
                    event: Box::new(event),
                }
            }
            #[cfg(windows)]
            ClientLoopEvent::StdinEvents(_) => ClientLoopEvent::OwnedInput {
                epoch: self.0,
                event: Box::new(event),
            },
            event => event,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn readable_kernel_input_never_adopts_new_modal_owner() {
        let epoch = AtomicU64::new(1);
        let owner = InputEpoch(1);
        let before_poll = InputEpoch::capture(&epoch);
        // poll reports readable; producer pauses; popup closes before read.
        epoch.store(2, Ordering::Release);
        assert_eq!(owner.after_poll(before_poll, true, false), owner);
        // Even a subsequent readable poll begun after close must drain the
        // remaining kernel queue with the old owner, not promote its secrets.
        assert_eq!(
            owner.after_poll(InputEpoch::capture(&epoch), true, false),
            owner
        );
        let ClientLoopEvent::OwnedInput { epoch: stamp, .. } =
            owner.own(ClientLoopEvent::StdinInput(b"queued secret".to_vec()))
        else {
            panic!("unowned input")
        };
        assert_ne!(stamp, epoch.load(Ordering::Acquire));
    }

    #[test]
    fn idle_boundary_uses_candidate_from_before_poll_not_after_close() {
        let epoch = AtomicU64::new(1);
        let owner = InputEpoch(1);
        let before_poll = InputEpoch::capture(&epoch);
        // No-data result, then popup closes before the producer resumes.
        epoch.store(2, Ordering::Release);
        let still_old = owner.after_poll(before_poll, false, false);
        assert_eq!(still_old, owner);
        let next_candidate = InputEpoch::capture(&epoch);
        assert_eq!(still_old.after_poll(next_candidate, false, true), owner);
        assert_eq!(
            still_old.after_poll(next_candidate, false, false),
            InputEpoch(2)
        );
    }

    #[test]
    fn pixel_reports_are_owned_but_resize_is_not() {
        let stamp = InputEpoch(12);
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(matches!(
            stamp.own(ClientLoopEvent::PixelMouse(
                b"\x1b[<0;1;1M".to_vec(),
                geometry
            )),
            ClientLoopEvent::OwnedInput { epoch: 12, .. }
        ));
        assert!(matches!(
            stamp.own(ClientLoopEvent::Resize(80, 24, 800, 480, false)),
            ClientLoopEvent::Resize(..)
        ));
    }
}
