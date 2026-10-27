use crate::config::Target;
use crate::date;
use crate::pins::PinsFile;
use serde_json::{Value, json};

struct Item {
    what: String,
    days: Option<i64>,
}

/// A flat summary of a pins file for dashboards, e.g. a homepage customapi
/// widget: how much is pinned or held, the oldest of it, and one status word.
///
/// status is the most urgent of: STALE (a pin or hold older than
/// warn_after_days), HELD, PINNED, OK.
pub fn summary(file: &PinsFile, today: i64, warn_after_days: i64) -> Value {
    let mut pins = 0;
    let mut held = 0;
    let mut items: Vec<Item> = Vec::new();
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name);
        if let Some(h) = &entry.held {
            held += 1;
            items.push(Item {
                what: format!("{} held", target.name),
                days: date::parse(&h.since).map(|d| today - d),
            });
        }
        for (_, attr, pin) in entry.iter() {
            pins += 1;
            items.push(Item {
                what: format!("{}: {attr}", target.name),
                days: date::parse(&pin.since).map(|d| today - d),
            });
        }
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.days.unwrap_or(i64::MAX)));

    let stale = items.iter().filter(|i| i.days.is_none_or(|d| d > warn_after_days)).count();
    let status = if stale > 0 {
        "STALE"
    } else if held > 0 {
        "HELD"
    } else if pins > 0 {
        "PINNED"
    } else {
        "OK"
    };
    let age = |i: &Item| i.days.map_or("?".to_string(), |d| format!("{d}d"));
    let oldest = items.first();
    let list = if items.is_empty() {
        "nothing pinned".to_string()
    } else {
        items.iter().map(|i| format!("{} ({})", i.what, age(i))).collect::<Vec<_>>().join(", ")
    };

    json!({
        "status": status,
        "pins": pins,
        "held": held,
        "stale": stale,
        "oldest": oldest.map_or("-".to_string(), |i| format!("{}, {}", i.what, age(i))),
        "oldest_days": oldest.and_then(|i| i.days).unwrap_or(0),
        "list": list,
        "warn_after_days": warn_after_days,
        "generated": date::format(today),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::{Held, Kind, Pin, Reason};

    fn pin(since: &str) -> Pin {
        Pin {
            locked: json!({ "narHash": "sha256-x" }),
            since: since.into(),
            reason: Reason::Eval,
            failed_drv: None,
            trail: Vec::new(),
            checked_nar_hash: String::new(),
        }
    }

    fn today() -> i64 {
        date::parse("2026-10-27").unwrap()
    }

    #[test]
    fn nothing_pinned_is_ok() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "ideapad");
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "OK");
        assert_eq!(s["pins"], 0);
        assert_eq!(s["oldest"], "-");
        assert_eq!(s["list"], "nothing pinned");
    }

    #[test]
    fn fresh_pins_are_pinned_and_oldest_first() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home").insert("nixpkgs", "open-webui", pin("2026-10-20"));
        f.entry_mut(Kind::Nixos, "pi").insert("nixpkgs", "sbctl", pin("2026-10-26"));
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "PINNED");
        assert_eq!(s["pins"], 2);
        assert_eq!(s["oldest"], "home: open-webui, 7d");
        assert_eq!(s["oldest_days"], 7);
        assert_eq!(s["list"], "home: open-webui (7d), pi: sbctl (1d)");
    }

    #[test]
    fn held_and_stale() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home").insert("nixpkgs", "open-webui", pin("2026-10-26"));
        f.entry_mut(Kind::Nixos, "pi").held = Some(Held {
            since: "2026-10-25".into(),
            reason: "linux fails".into(),
            lock: json!({}),
        });
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "HELD");
        assert_eq!(s["held"], 1);

        f.entry_mut(Kind::Home, "mikke").insert("nixpkgs", "zed-editor", pin("2026-10-01"));
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "STALE");
        assert_eq!(s["stale"], 1);
        assert_eq!(s["oldest"], "mikke: zed-editor, 26d");
    }
}
