//! Our GNOME custom keyboard shortcuts (gsettings, media-keys plugin).

use crate::Res;
use std::process::Command;

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const DIR: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/";
/// The launcher's shortcut's id ("screenrec", no arguments).
const LAUNCHER: &str = "screenrec";
/// The instant captures' shortcuts: id, arguments, the key they get unless picked already.
/// Screenshot of the screen, of the focused window, then their recordings.
pub const SNAPS: [(&str, &str, &str); 4] = [
    ("screenrec-snap", "snap", "<Shift>Print"),
    ("screenrec-snap-window", "snap --window", "<Alt>Print"),
    ("screenrec-rec", "snap --rec", "<Control><Shift>Print"),
    ("screenrec-rec-window", "snap --rec --window", "<Control><Alt>Print"),
];
/// GNOME's own screenshot tool's keys: Shell 42 and later, then settings-daemon before it.
const SCREENSHOT_KEYS: [(&str, &str); 7] = [
    ("org.gnome.shell.keybindings", "show-screenshot-ui"),
    ("org.gnome.shell.keybindings", "screenshot"),
    ("org.gnome.shell.keybindings", "screenshot-window"),
    ("org.gnome.shell.keybindings", "show-screen-recording-ui"),
    (SCHEMA, "screenshot"),
    (SCHEMA, "window-screenshot"),
    (SCHEMA, "area-screenshot"),
];

fn gsettings(args: &[&str]) -> Res<String> {
    let out = Command::new("gsettings").args(args).output()?;
    if !out.status.success() {
        return Err(format!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn path(id: &str) -> String {
    format!("{DIR}{id}/")
}

fn ours(id: &str) -> String {
    format!("{SCHEMA}.custom-keybinding:{}", path(id))
}

/// The launcher's binding as a GNOME accelerator ("minus", "<Control><Alt>s"), if it has one.
pub fn get() -> Option<String> {
    get_of(LAUNCHER)
}

/// Shortcut `id`'s binding, if we made it.
pub fn get_of(id: &str) -> Option<String> {
    gsettings(&["get", SCHEMA, "custom-keybindings"]).ok().filter(|l| l.contains(&path(id)))?;
    Some(gsettings(&["get", &ours(id), "binding"]).ok()?.trim_matches('\'').to_owned())
}

/// Bind `accel` to the launcher.
pub fn set(accel: &str) -> Res<()> {
    bind(LAUNCHER, "", accel)
}

/// Bind `accel` to run this executable with `args`, as our shortcut `id`, adding it to
/// the custom shortcuts list without touching the others. If GNOME's screenshot tool
/// has `accel` (Print), it loses it: ours overrides it.
pub fn bind(id: &str, args: &str, accel: &str) -> Res<()> {
    let list = gsettings(&["get", SCHEMA, "custom-keybindings"])?;
    if !list.contains(&path(id)) {
        let me = format!("'{}'", path(id));
        let mut items = items(&list);
        items.push(&me);
        gsettings(&["set", SCHEMA, "custom-keybindings", &format!("[{}]", items.join(", "))])?;
    }
    // GNOME's screenshot tool lets go of the key (Print, say), or it keeps it and ours never fires.
    let me = format!("'{accel}'");
    for (schema, key) in SCREENSHOT_KEYS {
        let Ok(list) = gsettings(&["get", schema, key]) else { continue }; // not a key of this GNOME
        let keys = items(&list);
        if list.contains('[') && keys.contains(&me.as_str()) {
            let left: Vec<&str> = keys.into_iter().filter(|k| *k != me).collect();
            gsettings(&["set", schema, key, &format!("[{}]", left.join(", "))])?;
        }
    }
    let exe = std::env::current_exe()?;
    let run = format!("{} {args}", exe.display());
    gsettings(&["set", &ours(id), "name", &format!("'screenrec {args}'").replace(" '", "'")])?;
    gsettings(&["set", &ours(id), "command", &format!("'{}'", run.trim_end())])?;
    gsettings(&["set", &ours(id), "binding", &format!("'{accel}'")])?;
    Ok(())
}

/// The items of a gsettings string list ("@as []", "['a', 'b']"), quotes kept.
fn items(list: &str) -> Vec<&str> {
    let inner = list.trim_start_matches("@as").trim().trim_start_matches('[').trim_end_matches(']');
    inner.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
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
        assert_eq!(items("@as []"), Vec::<&str>::new());
        assert_eq!(items("['Print', '<Shift>Print']"), ["'Print'", "'<Shift>Print'"]);
    }
}
