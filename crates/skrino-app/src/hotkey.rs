//! Parse a human hotkey string (e.g. "PrintScreen", "Ctrl+Shift+S") into a
//! `global_hotkey::hotkey::HotKey`, and manage (re)registration.

use global_hotkey::{
    GlobalHotKeyManager,
    hotkey::{Code, HotKey, Modifiers},
};

/// Parse a "+"-separated hotkey string. Modifiers in any order, the last token
/// is the key. Case-insensitive. Returns a human-readable error on failure.
pub fn parse(spec: &str) -> Result<HotKey, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("пустая комбинация".into());
    }

    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;

    for raw in spec.split('+') {
        let tok = raw.trim();
        if tok.is_empty() {
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" | "ctl" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "meta" | "super" | "win" | "windows" | "cmd" | "command" => mods |= Modifiers::META,
            _ => {
                if key.is_some() {
                    return Err(format!("несколько основных клавиш в «{spec}»"));
                }
                key = Some(parse_code(&lower).ok_or_else(|| format!("неизвестная клавиша «{tok}»"))?);
            }
        }
    }

    let key = key.ok_or_else(|| "не указана основная клавиша".to_string())?;
    let mods = if mods.is_empty() { None } else { Some(mods) };
    Ok(HotKey::new(mods, key))
}

fn parse_code(lower: &str) -> Option<Code> {
    // Single letter a-z.
    if lower.len() == 1 {
        let c = lower.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some(match c {
                'a' => Code::KeyA, 'b' => Code::KeyB, 'c' => Code::KeyC, 'd' => Code::KeyD,
                'e' => Code::KeyE, 'f' => Code::KeyF, 'g' => Code::KeyG, 'h' => Code::KeyH,
                'i' => Code::KeyI, 'j' => Code::KeyJ, 'k' => Code::KeyK, 'l' => Code::KeyL,
                'm' => Code::KeyM, 'n' => Code::KeyN, 'o' => Code::KeyO, 'p' => Code::KeyP,
                'q' => Code::KeyQ, 'r' => Code::KeyR, 's' => Code::KeyS, 't' => Code::KeyT,
                'u' => Code::KeyU, 'v' => Code::KeyV, 'w' => Code::KeyW, 'x' => Code::KeyX,
                'y' => Code::KeyY, _ => Code::KeyZ,
            });
        }
        if c.is_ascii_digit() {
            return Some(match c {
                '0' => Code::Digit0, '1' => Code::Digit1, '2' => Code::Digit2, '3' => Code::Digit3,
                '4' => Code::Digit4, '5' => Code::Digit5, '6' => Code::Digit6, '7' => Code::Digit7,
                '8' => Code::Digit8, _ => Code::Digit9,
            });
        }
    }

    // Function keys F1..F12.
    if let Some(n) = lower.strip_prefix('f').and_then(|r| r.parse::<u8>().ok()) {
        return Some(match n {
            1 => Code::F1, 2 => Code::F2, 3 => Code::F3, 4 => Code::F4,
            5 => Code::F5, 6 => Code::F6, 7 => Code::F7, 8 => Code::F8,
            9 => Code::F9, 10 => Code::F10, 11 => Code::F11, 12 => Code::F12,
            _ => return None,
        });
    }

    Some(match lower {
        "printscreen" | "prtsc" | "prtscr" | "prntscrn" | "print" | "snapshot" => Code::PrintScreen,
        "space" | "spacebar" => Code::Space,
        "enter" | "return" => Code::Enter,
        "esc" | "escape" => Code::Escape,
        "tab" => Code::Tab,
        "backspace" => Code::Backspace,
        "delete" | "del" => Code::Delete,
        "insert" | "ins" => Code::Insert,
        "home" => Code::Home,
        "end" => Code::End,
        "pageup" | "pgup" => Code::PageUp,
        "pagedown" | "pgdn" => Code::PageDown,
        "up" | "arrowup" => Code::ArrowUp,
        "down" | "arrowdown" => Code::ArrowDown,
        "left" | "arrowleft" => Code::ArrowLeft,
        "right" | "arrowright" => Code::ArrowRight,
        _ => return None,
    })
}

/// Registers the current hotkey and unregisters the previous one on change.
pub struct HotkeyRegistration {
    manager: GlobalHotKeyManager,
    current: Option<HotKey>,
}

impl HotkeyRegistration {
    pub fn new() -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        Ok(Self {
            manager,
            current: None,
        })
    }

    /// The id of the currently registered hotkey (to match incoming events).
    pub fn current_id(&self) -> Option<u32> {
        self.current.map(|h| h.id())
    }

    /// Register `spec`, replacing any previously registered hotkey.
    pub fn set(&mut self, spec: &str) -> Result<(), String> {
        let hk = parse(spec)?;
        if self.current == Some(hk) {
            return Ok(());
        }
        if let Some(old) = self.current.take() {
            let _ = self.manager.unregister(old);
        }
        self.manager.register(hk).map_err(|e| e.to_string())?;
        self.current = Some(hk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_printscreen() {
        let hk = parse("PrintScreen").unwrap();
        assert_eq!(hk, HotKey::new(None, Code::PrintScreen));
    }

    #[test]
    fn parses_ctrl_shift_s_any_case() {
        let hk = parse("ctrl+SHIFT+s").unwrap();
        assert_eq!(
            hk,
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS)
        );
    }

    #[test]
    fn parses_function_key() {
        assert_eq!(parse("F12").unwrap(), HotKey::new(None, Code::F12));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("Ctrl+").is_err());
        assert!(parse("Nope").is_err());
        assert!(parse("Ctrl+A+B").is_err());
    }
}
