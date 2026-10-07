//! Our GNOME custom keyboard shortcut (gsettings, media-keys plugin).

use crate::Res;
use std::process::Command;

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/screenrec/";

fn gsettings(args: &[&str]) -> Res<String> {
    let out = Command::new("gsettings").args(args).output()?;
    if !out.status.success() {
        return Err(format!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn ours() -> String {
    format!("{SCHEMA}.custom-keybinding:{PATH}")
}

/// Our binding as a GNOME accelerator ("minus", "<Control><Alt>s"), if we have one.
pub fn get() -> Option<String> {
    gsettings(&["get", SCHEMA, "custom-keybindings"]).ok().filter(|l| l.contains(PATH))?;
    Some(gsettings(&["get", &ours(), "binding"]).ok()?.trim_matches('\'').to_owned())
}

/// Bind `accel` to launch this executable, adding our entry to the custom
/// shortcuts list without touching the others.
pub fn set(accel: &str) -> Res<()> {
    let list = gsettings(&["get", SCHEMA, "custom-keybindings"])?;
    if !list.contains(PATH) {
        let inner = list.trim_start_matches("@as").trim().trim_start_matches('[').trim_end_matches(']');
        let mut items: Vec<&str> = inner.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let me = format!("'{PATH}'");
        items.push(&me);
        gsettings(&["set", SCHEMA, "custom-keybindings", &format!("[{}]", items.join(", "))])?;
    }
    let exe = std::env::current_exe()?;
    gsettings(&["set", &ours(), "name", "'screenrec'"])?;
    gsettings(&["set", &ours(), "command", &format!("'{}'", exe.display())])?;
    gsettings(&["set", &ours(), "binding", &format!("'{accel}'")])?;
    Ok(())
}

/// Whether GNOME's "Reduce animation" is on (no gsettings or schema: no).
pub fn animations_off() -> bool {
    gsettings(&["get", "org.gnome.desktop.interface", "enable-animations"]).is_ok_and(|v| v == "false")
}

/// GNOME accelerator for a key press: unshifted keysym plus modifier mask.
/// None for a lone modifier (wait for the real key).
pub fn accel(keysym: u32, state: u16) -> Option<String> {
    let ks = xkeysym::Keysym::new(keysym);
    if ks.is_modifier_key() {
        return None;
    }
    let name = ks.name()?.strip_prefix("XK_")?;
    let mods = [(4, "<Control>"), (8, "<Alt>"), (1, "<Shift>"), (64, "<Super>")];
    Some(mods.iter().filter(|(bit, _)| state & bit != 0).map(|(_, m)| *m).collect::<String>() + name)
}

/// "<Control><Alt>s" -> "Ctrl+Alt+S", "minus" -> "-".
pub fn pretty(accel: &str) -> String {
    let mut parts = vec![];
    let mut rest = accel;
    while let Some((m, r)) = rest.strip_prefix('<').and_then(|r| r.split_once('>')) {
        parts.push(if m == "Control" || m == "Primary" { "Ctrl".to_owned() } else { m.to_owned() });
        rest = r;
    }
    parts.push(match rest {
        "minus" => "-".to_owned(),
        "plus" => "+".to_owned(),
        "equal" => "=".to_owned(),
        k if k.chars().count() == 1 => k.to_uppercase(),
        k => k.to_owned(),
    });
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerators() {
        assert_eq!(accel(0x2d, 0).as_deref(), Some("minus"));
        assert_eq!(accel(0x73, 4 | 8).as_deref(), Some("<Control><Alt>s"));
        assert_eq!(accel(0xffe1, 1), None); // Shift_L alone
        assert_eq!(pretty("<Control><Alt>s"), "Ctrl+Alt+S");
        assert_eq!(pretty("minus"), "-");
        assert_eq!(pretty("<Super>Print"), "Super+Print");
    }
}
