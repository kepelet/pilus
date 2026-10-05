//! Translate a GPUI keystroke into the wire's key event.
//!
//! The daemon speaks crossterm's `KeyEvent`, so every key the window receives
//! is mapped back into that vocabulary before it is forwarded.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gpui::{Keystroke, Modifiers};

/// Map a GPUI [`Keystroke`] to a crossterm key press.
///
/// Returns `None` for modifier-only keystrokes, which carry no input.
pub fn to_wire(keystroke: &Keystroke, kind: KeyEventKind) -> Option<KeyEvent> {
    let code = key_code(keystroke)?;
    let mut modifiers = modifiers(keystroke.modifiers);
    // Option already went into the glyph, so forwarding Alt as well would
    // have the daemon send a Meta chord (`ESC` + char) instead of the text.
    if option_composed(keystroke) {
        modifiers.remove(KeyModifiers::ALT);
    }
    Some(KeyEvent::new_with_kind(code, modifiers, kind))
}

/// Whether Option produced a different character from the bare key, the way
/// macOS types `—` for Option+Shift+- or `™` for Option+2. Named keys
/// (`space`, `tab`, …) are excluded: their `key_char` never comes from the
/// layout's Option level.
fn option_composed(keystroke: &Keystroke) -> bool {
    let mods = keystroke.modifiers;
    mods.alt
        && !mods.control
        && !mods.platform
        && keystroke.key.chars().count() == 1
        && keystroke
            .key_char
            .as_deref()
            .is_some_and(|typed| !typed.is_empty() && typed != keystroke.key)
}

fn key_code(keystroke: &Keystroke) -> Option<KeyCode> {
    let key = keystroke.key.as_str();
    let code = match key {
        "enter" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        // GPUI reports Shift+Tab as `tab` with Shift held. crossterm, and so
        // the daemon, calls that key `BackTab` and encodes it as `CSI Z`; a
        // shifted `Tab` would reach the pane as a plain tab.
        "tab" if keystroke.modifiers.shift => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "null" => KeyCode::Null,
        name if name.len() >= 2 && name.starts_with('f') => {
            let number: u8 = name[1..].parse().ok()?;
            (1..=12).contains(&number).then_some(KeyCode::F(number))?
        }
        _ => {
            // Prefer the character the keystroke would type, so shifted and
            // composed symbols arrive as the user sees them.
            let typed = keystroke
                .key_char
                .as_deref()
                .filter(|text| !text.is_empty())
                .or(Some(key));
            let mut chars = typed?.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(first)
        }
    };
    Some(code)
}

/// Translate pointer modifiers into the daemon's vocabulary.
///
/// The daemon opens a pane link on `Ctrl`+click. macOS terminals use `Cmd`
/// for the same gesture, so alias `Cmd` onto `Ctrl` here and let the daemon
/// see the chord it already understands.
pub(super) fn mouse_modifiers(modifiers: Modifiers) -> KeyModifiers {
    let mut out = self::modifiers(modifiers);
    if modifiers.platform {
        out |= KeyModifiers::CONTROL;
    }
    out
}

pub(super) fn modifiers(modifiers: Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if modifiers.control {
        out |= KeyModifiers::CONTROL;
    }
    if modifiers.alt {
        out |= KeyModifiers::ALT;
    }
    if modifiers.shift {
        out |= KeyModifiers::SHIFT;
    }
    if modifiers.platform {
        out |= KeyModifiers::SUPER;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: &str, char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: key.to_string(),
            key_char: char.map(str::to_string),
        }
    }

    #[test]
    fn named_keys_map() {
        assert_eq!(
            to_wire(&stroke("enter", None), KeyEventKind::Press)
                .expect("enter")
                .code,
            KeyCode::Enter
        );
        assert_eq!(
            to_wire(&stroke("f5", None), KeyEventKind::Press)
                .expect("f5")
                .code,
            KeyCode::F(5)
        );
    }

    #[test]
    fn character_uses_typed_char() {
        let event = to_wire(&stroke("s", Some("ß")), KeyEventKind::Press).expect("char");
        assert_eq!(event.code, KeyCode::Char('ß'));
    }

    #[test]
    fn shift_tab_is_backtab() {
        let mut shift_tab = stroke("tab", None);
        shift_tab.modifiers.shift = true;
        let event = to_wire(&shift_tab, KeyEventKind::Press).expect("backtab");
        assert_eq!(event.code, KeyCode::BackTab);
        // crossterm reports BackTab with Shift still held.
        assert!(event.modifiers.contains(KeyModifiers::SHIFT));
        assert_eq!(
            to_wire(&stroke("tab", Some("\t")), KeyEventKind::Press)
                .expect("tab")
                .code,
            KeyCode::Tab
        );
    }

    #[test]
    fn control_letters_keep_their_letter() {
        // GPUI drops `key_char` under Control, so Ctrl+T must still name `t`.
        let mut ctrl_t = stroke("t", None);
        ctrl_t.modifiers.control = true;
        let event = to_wire(&ctrl_t, KeyEventKind::Press).expect("ctrl-t");
        assert_eq!(event.code, KeyCode::Char('t'));
        assert!(event.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn option_composed_characters_arrive_as_text() {
        // Option+Shift+- on a US layout: GPUI reports `_` with Alt held and
        // the em dash as the typed character.
        let mut em_dash = stroke("_", Some("—"));
        em_dash.modifiers.alt = true;
        let event = to_wire(&em_dash, KeyEventKind::Press).expect("em dash");
        assert_eq!(event.code, KeyCode::Char('—'));
        assert!(!event.modifiers.contains(KeyModifiers::ALT));

        // Option that composes nothing stays a Meta chord.
        let mut meta = stroke("x", Some("x"));
        meta.modifiers.alt = true;
        let event = to_wire(&meta, KeyEventKind::Press).expect("meta");
        assert!(event.modifiers.contains(KeyModifiers::ALT));

        // A named key keeps Alt even though its `key_char` differs.
        let mut alt_space = stroke("space", Some(" "));
        alt_space.modifiers.alt = true;
        let event = to_wire(&alt_space, KeyEventKind::Press).expect("alt space");
        assert!(event.modifiers.contains(KeyModifiers::ALT));
    }

    #[test]
    fn modifier_only_is_dropped() {
        assert!(to_wire(&stroke("shift", None), KeyEventKind::Press).is_none());
    }

    #[test]
    fn command_aliases_onto_control_for_the_mouse() {
        // The daemon opens links on Ctrl+click; macOS users reach for Cmd.
        let command = mouse_modifiers(Modifiers {
            platform: true,
            ..Modifiers::default()
        });
        assert!(command.contains(KeyModifiers::CONTROL));
        assert!(mouse_modifiers(Modifiers::default()).is_empty());
    }
}
