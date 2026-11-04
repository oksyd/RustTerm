use gpui::Modifiers;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Idle,
    First,
    Armed,
    Releasing,
    Blocked,
}

/// Recognizes a standalone, sequential Ctrl+Alt chord when both keys are released.
#[derive(Default)]
pub(crate) struct ReleaseGesture {
    state: State,
}

impl ReleaseGesture {
    /// A simultaneous Ctrl+Alt press is ambiguous with AltGr and never arms the gesture.
    /// The caller must release any input already forwarded to the server on `true`.
    pub(crate) fn modifiers_changed(&mut self, previous: Modifiers, current: Modifiers) -> bool {
        if !current.modified() {
            let released = match self.state {
                State::Armed => previous.control && previous.alt,
                State::Releasing => previous.control ^ previous.alt,
                _ => false,
            } && !other_modifiers(previous);
            self.state = State::Idle;
            return released;
        }
        if other_modifiers(previous) || other_modifiers(current) {
            self.state = State::Blocked;
            return false;
        }
        if previous == current {
            return false;
        }
        self.state = match self.state {
            State::Idle if !previous.modified() && (current.control ^ current.alt) => State::First,
            State::First if (previous.control ^ previous.alt) && current.control && current.alt => {
                State::Armed
            }
            State::Armed if previous.control && previous.alt && (current.control ^ current.alt) => {
                State::Releasing
            }
            _ => State::Blocked,
        };
        false
    }

    /// Interrupt on other key presses or pointer activity, including while idle.
    /// Reapply this before modifier changes while any other key/button remains held.
    pub(crate) fn interrupt(&mut self) {
        self.state = State::Blocked;
    }

    /// Clear after capture ends, or after all physical keys/buttons have been released.
    /// Do not reset an interrupted gesture while Ctrl or Alt is still held.
    pub(crate) fn reset(&mut self) {
        self.state = State::Idle;
    }
}

fn other_modifiers(modifiers: Modifiers) -> bool {
    modifiers.shift || modifiers.platform || modifiers.function
}

/// Tracks evdev keys independently of layout changes, text composition and repeats.
#[derive(Default)]
pub(crate) struct PhysicalReleaseGesture {
    keys: BTreeSet<u32>,
    gesture: ReleaseGesture,
}

impl PhysicalReleaseGesture {
    pub(crate) fn has_held_keys(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Recognize either Control key with left Alt. Right Alt is always excluded.
    pub(crate) fn key_changed(&mut self, code: u32, pressed: bool) -> bool {
        let previous = self.modifiers();
        let changed = if pressed {
            self.keys.insert(code)
        } else {
            self.keys.remove(&code)
        };
        if !changed {
            return false;
        }

        if !chord_key(code) {
            self.gesture.interrupt();
            if self.keys.is_empty() {
                self.gesture.reset();
                return false;
            }
        }
        if self.keys.iter().any(|code| !chord_key(*code)) {
            self.gesture.interrupt();
            return false;
        }
        let current = self.modifiers();
        self.gesture.modifiers_changed(previous, current)
    }

    /// Cancel a chord without making an idle pointer event block the next chord.
    pub(crate) fn interrupt(&mut self) {
        if self.keys.is_empty() {
            self.gesture.reset();
        } else {
            self.gesture.interrupt();
        }
    }

    /// Forget all input when its native keyboard loses focus or is released.
    pub(crate) fn reset(&mut self) {
        self.keys.clear();
        self.gesture.reset();
    }

    /// Adopt the keys held on keyboard enter without recognizing their release.
    pub(crate) fn prime(&mut self, keys: &[u32]) {
        self.reset();
        self.keys.extend(keys);
        self.interrupt();
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers {
            control: self.keys.contains(&29) || self.keys.contains(&97),
            alt: self.keys.contains(&56),
            ..Default::default()
        }
    }
}

fn chord_key(code: u32) -> bool {
    matches!(code, 29 | 97 | 56)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(control: bool, alt: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            ..Default::default()
        }
    }

    fn step(gesture: &mut ReleaseGesture, previous: &mut Modifiers, current: Modifiers) -> bool {
        let release = gesture.modifiers_changed(*previous, current);
        *previous = current;
        release
    }

    #[test]
    fn bare_chord_releases_after_both_keys_in_either_order() {
        for first in [modifiers(true, false), modifiers(false, true)] {
            for remaining in [modifiers(true, false), modifiers(false, true)] {
                let mut gesture = ReleaseGesture::default();
                let mut previous = Modifiers::default();
                assert!(!step(&mut gesture, &mut previous, first));
                assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
                assert!(!step(&mut gesture, &mut previous, remaining));
                assert!(step(&mut gesture, &mut previous, Modifiers::default()));
                assert!(!step(&mut gesture, &mut previous, Modifiers::default()));
            }
        }
    }

    #[test]
    fn coalesced_release_is_allowed_but_coalesced_press_is_not() {
        let mut gesture = ReleaseGesture::default();
        let mut previous = Modifiers::default();
        assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
        assert!(!step(&mut gesture, &mut previous, modifiers(false, true)));
        assert!(!step(&mut gesture, &mut previous, Modifiers::default()));

        assert!(!step(&mut gesture, &mut previous, modifiers(true, false)));
        assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
        assert!(step(&mut gesture, &mut previous, Modifiers::default()));
    }

    #[test]
    fn other_input_cancels_every_phase_until_all_modifiers_are_released() {
        let sequence = [
            modifiers(true, false),
            modifiers(true, true),
            modifiers(false, true),
            Modifiers::default(),
        ];
        for interrupted_phase in 0..=3 {
            let mut gesture = ReleaseGesture::default();
            let mut previous = Modifiers::default();
            for (phase, current) in sequence.into_iter().enumerate() {
                if phase == interrupted_phase {
                    gesture.interrupt();
                }
                assert!(!step(&mut gesture, &mut previous, current));
            }
            assert!(!step(&mut gesture, &mut previous, modifiers(false, true)));
            assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
            assert!(step(&mut gesture, &mut previous, Modifiers::default()));
        }
    }

    #[test]
    fn shift_super_or_function_prevents_a_standalone_chord() {
        for extra in [
            Modifiers {
                shift: true,
                ..Default::default()
            },
            Modifiers {
                platform: true,
                ..Default::default()
            },
            Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            let mut gesture = ReleaseGesture::default();
            let mut previous = Modifiers::default();
            assert!(!step(&mut gesture, &mut previous, modifiers(true, false)));
            assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
            assert!(!step(
                &mut gesture,
                &mut previous,
                Modifiers {
                    control: true,
                    alt: true,
                    ..extra
                }
            ));
            assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
            assert!(!step(&mut gesture, &mut previous, Modifiers::default()));
        }
    }

    #[test]
    fn repressing_a_released_modifier_does_not_restart_the_gesture() {
        let mut gesture = ReleaseGesture::default();
        let mut previous = Modifiers::default();
        for current in [
            modifiers(true, false),
            modifiers(true, true),
            modifiers(false, true),
            modifiers(true, true),
            Modifiers::default(),
        ] {
            assert!(!step(&mut gesture, &mut previous, current));
        }
    }

    #[test]
    fn repeated_modifier_notifications_do_not_cancel_a_bare_chord() {
        let mut gesture = ReleaseGesture::default();
        let mut previous = Modifiers::default();
        for current in [
            modifiers(true, false),
            modifiers(true, false),
            modifiers(true, true),
            modifiers(true, true),
            modifiers(true, false),
            modifiers(true, false),
        ] {
            assert!(!step(&mut gesture, &mut previous, current));
        }
        assert!(step(&mut gesture, &mut previous, Modifiers::default()));
    }

    #[test]
    fn reset_does_not_turn_an_already_held_chord_into_a_gesture() {
        let mut gesture = ReleaseGesture::default();
        let mut previous = Modifiers::default();
        assert!(!step(&mut gesture, &mut previous, modifiers(true, false)));
        assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
        gesture.reset();
        assert!(!step(&mut gesture, &mut previous, modifiers(true, false)));
        assert!(!step(&mut gesture, &mut previous, Modifiers::default()));

        gesture.interrupt();
        // The caller has observed all non-modifier keys and pointer buttons released.
        gesture.reset();
        assert!(!step(&mut gesture, &mut previous, modifiers(false, true)));
        assert!(!step(&mut gesture, &mut previous, modifiers(true, true)));
        assert!(step(&mut gesture, &mut previous, Modifiers::default()));
    }

    fn physical_chord(gesture: &mut PhysicalReleaseGesture) {
        assert!(!gesture.key_changed(29, true));
        assert!(!gesture.key_changed(56, true));
        assert!(!gesture.key_changed(29, false));
        assert!(gesture.key_changed(56, false));
    }

    #[test]
    fn physical_chord_accepts_either_control_and_both_orders() {
        for control in [29, 97] {
            for press in [[control, 56], [56, control]] {
                for release in [[control, 56], [56, control]] {
                    let mut gesture = PhysicalReleaseGesture::default();
                    for code in press {
                        assert!(!gesture.key_changed(code, true));
                    }
                    assert!(!gesture.key_changed(release[0], false));
                    assert!(gesture.key_changed(release[1], false));
                }
            }
        }
    }

    #[test]
    fn physical_other_keys_block_until_released_without_poisoning_next_chord() {
        // A normal key, Shift, Super and AltGr all exclude a standalone chord.
        for other in [30, 42, 125, 100] {
            let mut gesture = PhysicalReleaseGesture::default();
            assert!(!gesture.key_changed(other, true));
            for (code, pressed) in [(29, true), (56, true), (29, false), (56, false)] {
                assert!(!gesture.key_changed(code, pressed));
            }
            // Releasing Ctrl+Alt while another key is held must not rearm it.
            for (code, pressed) in [(56, true), (29, true), (56, false), (29, false)] {
                assert!(!gesture.key_changed(code, pressed));
            }
            assert!(!gesture.key_changed(other, false));
            physical_chord(&mut gesture);

            assert!(!gesture.key_changed(29, true));
            assert!(!gesture.key_changed(56, true));
            assert!(!gesture.key_changed(other, true));
            assert!(!gesture.key_changed(other, false));
            assert!(!gesture.key_changed(29, false));
            assert!(!gesture.key_changed(56, false));
            physical_chord(&mut gesture);
        }
    }

    #[test]
    fn physical_symbol_identity_does_not_depend_on_shift_state() {
        let mut gesture = PhysicalReleaseGesture::default();
        // Shift+1 becomes plain 1 before release; both events still use code 2.
        for (code, pressed) in [(42, true), (2, true), (42, false), (2, false)] {
            assert!(!gesture.key_changed(code, pressed));
        }
        physical_chord(&mut gesture);
    }

    #[test]
    fn physical_repeats_and_unmatched_releases_are_ignored() {
        let mut gesture = PhysicalReleaseGesture::default();
        assert!(!gesture.key_changed(29, true));
        assert!(!gesture.key_changed(29, true));
        assert!(!gesture.key_changed(56, true));
        assert!(!gesture.key_changed(56, true));
        assert!(!gesture.key_changed(30, false));
        assert!(!gesture.key_changed(29, false));
        assert!(!gesture.key_changed(29, false));
        assert!(gesture.key_changed(56, false));
        assert!(!gesture.key_changed(56, false));
    }

    #[test]
    fn physical_enter_keys_cannot_complete_a_release_gesture() {
        for keys in [&[29, 56][..], &[29][..], &[100, 30][..], &[42, 2][..]] {
            let mut gesture = PhysicalReleaseGesture::default();
            gesture.prime(keys);
            for &code in keys {
                assert!(!gesture.key_changed(code, false));
            }
            physical_chord(&mut gesture);
        }

        let mut gesture = PhysicalReleaseGesture::default();
        gesture.prime(&[29]);
        assert!(!gesture.key_changed(56, true));
        assert!(!gesture.key_changed(29, false));
        assert!(!gesture.key_changed(56, false));
        physical_chord(&mut gesture);
    }

    #[test]
    fn physical_pointer_activity_cancels_only_the_current_chord() {
        let mut gesture = PhysicalReleaseGesture::default();
        gesture.interrupt();
        physical_chord(&mut gesture);

        assert!(!gesture.key_changed(29, true));
        assert!(!gesture.key_changed(56, true));
        gesture.interrupt();
        assert!(!gesture.key_changed(29, false));
        assert!(!gesture.key_changed(56, false));
        physical_chord(&mut gesture);
    }

    #[test]
    fn physical_reset_discards_previous_keyboard_state() {
        let mut gesture = PhysicalReleaseGesture::default();
        gesture.prime(&[29, 56, 30]);
        gesture.reset();
        assert!(!gesture.key_changed(30, false));
        physical_chord(&mut gesture);
    }
}
