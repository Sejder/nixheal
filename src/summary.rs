use crate::config::Target;
use crate::date;
use crate::pins::PinsFile;
use serde_json::{Value, json};

struct Item {
    what: String,
    kind: &'static str,
    days: Option<i64>,
    /// Structured fields for scripts, next to the display strings.
    fields: Value,
}

/// A flat summary of a pins file for dashboards, e.g. a homepage customapi
/// widget: how much is pinned or held, the oldest of it, and one status word.
///
/// status is the most urgent of: STALE (a pin or hold older than
/// warn_after_days), HELD, PINNED, OK.
pub fn summary(file: &PinsFile, today: i64, warn_after_days: i64) -> Value {
    let mut pins = 0;
    let mut pin_entries = 0;
    let mut held = 0;
    let mut items: Vec<Item> = Vec::new();
    for (kind, name, entry) in file.entries() {
        let target = Target::new(kind, name);
        if let Some(h) = &entry.held {
            held += 1;
            items.push(Item {
                what: format!("{target} held"),
                kind: "held",
                days: date::parse(&h.since).map(|d| today - d),
                fields: json!({
                    "target": name,
                    "target_kind": kind.noun(),
                    "since": h.since,
                    "held_reason": h.reason,
                }),
            });
        }
        for (attr, sets) in entry.packages() {
            pins += 1;
            pin_entries += sets.len();
            let pin = sets[0].1;
            let inputs: Vec<&str> = sets.iter().map(|(i, _)| *i).collect();
            items.push(Item {
                what: format!("{target}: {attr}"),
                kind: pin.reason.as_str(),
                days: date::parse(&pin.since).map(|d| today - d),
                fields: json!({
                    "target": name,
                    "target_kind": kind.noun(),
                    "inputs": inputs,
                    "attr": attr,
                    "since": pin.since,
                }),
            });
        }
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.days.unwrap_or(i64::MAX)));

    let is_stale = |i: &Item| i.kind != "manual" && i.days.is_none_or(|d| d > warn_after_days);
    let stale = items.iter().filter(|i| is_stale(i)).count();
    let status = if stale > 0 {
        "STALE"
    } else if held > 0 {
        "HELD"
    } else if pins > 0 {
        "PINNED"
    } else {
        "OK"
    };
    let age = |i: &Item| match i.days {
        Some(0) => "today".to_string(),
        Some(d) => format!("{d}d"),
        None => "?".to_string(),
    };
    let oldest = items.first();
    let list = if items.is_empty() {
        "nothing pinned".to_string()
    } else {
        items
            .iter()
            .map(|i| format!("{} ({})", i.what, age(i)))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // One row per pin or hold, for a dynamic-list widget; never empty, so the
    // widget shows that all is well instead of nothing.
    let rows: Vec<Value> = if items.is_empty() {
        vec![
            json!({ "name": "nothing pinned", "label": "OK", "kind": "none", "days": 0, "stale": false }),
        ]
    } else {
        items
            .iter()
            .map(|i| {
                let mut label = format!("{}, {}", age(i), i.kind);
                if is_stale(i) {
                    label.push_str(", STALE");
                }
                let mut row = json!({
                    "name": i.what,
                    "label": label,
                    "kind": i.kind,
                    "days": i.days,
                    "stale": is_stale(i),
                });
                if let (Some(row), Some(fields)) = (row.as_object_mut(), i.fields.as_object()) {
                    row.extend(fields.clone());
                }
                row
            })
            .collect()
    };

    json!({
        "status": status,
        "pins": pins,
        "pin_entries": pin_entries,
        "held": held,
        "stale": stale,
        "oldest": oldest.map_or("-".to_string(), |i| format!("{}, {}", i.what, age(i))),
        "oldest_days": oldest.and_then(|i| i.days).unwrap_or(0),
        "list": list,
        "items": rows,
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
            error: None,
            overlays: None,
            checked_nar_hash: String::new(),
        }
    }

    fn today() -> i64 {
        date::parse("2026-10-27").unwrap()
    }

    #[test]
    fn nothing_pinned_is_ok() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "laptop");
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "OK");
        assert_eq!(s["pins"], 0);
        assert_eq!(s["oldest"], "-");
        assert_eq!(s["list"], "nothing pinned");
        assert_eq!(s["items"][0]["name"], "nothing pinned");
        assert_eq!(s["items"][0]["label"], "OK");
    }

    #[test]
    fn fresh_pins_are_pinned_and_oldest_first() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home")
            .insert("nixpkgs", "open-webui", pin("2026-10-20"));
        f.entry_mut(Kind::Nixos, "pi")
            .insert("nixpkgs", "sbctl", pin("2026-10-26"));
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "PINNED");
        assert_eq!(s["pins"], 2);
        assert_eq!(s["oldest"], "host home: open-webui, 7d");
        assert_eq!(s["oldest_days"], 7);
        assert_eq!(s["list"], "host home: open-webui (7d), host pi: sbctl (1d)");
        assert_eq!(s["items"][0]["name"], "host home: open-webui");
        assert_eq!(s["items"][0]["label"], "7d, eval");
        assert_eq!(s["items"][1]["name"], "host pi: sbctl");
        assert_eq!(s["items"].as_array().unwrap().len(), 2);

        f.entry_mut(Kind::Nixos, "pi")
            .insert("nixpkgs-unstable", "sbctl", pin("2026-10-26"));
        let s = summary(&f, today(), 14);
        assert_eq!(s["pins"], 2);
        assert_eq!(s["pin_entries"], 3);
        assert_eq!(
            s["items"][1]["inputs"],
            json!(["nixpkgs", "nixpkgs-unstable"])
        );
    }

    #[test]
    fn held_and_stale() {
        let mut f = PinsFile::default();
        f.entry_mut(Kind::Nixos, "home")
            .insert("nixpkgs", "open-webui", pin("2026-10-26"));
        f.entry_mut(Kind::Nixos, "pi").held = Some(Held {
            since: "2026-10-25".into(),
            reason: "linux fails".into(),
            lock: json!({}),
        });
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "HELD");
        assert_eq!(s["held"], 1);

        f.entry_mut(Kind::Home, "alice")
            .insert("nixpkgs", "zed-editor", pin("2026-10-01"));
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "STALE");
        assert_eq!(s["stale"], 1);
        assert_eq!(s["oldest"], "home alice: zed-editor, 26d");
        assert_eq!(s["items"][0]["label"], "26d, eval, STALE");
        assert_eq!(s["items"][0]["name"], "home alice: zed-editor");
        assert_eq!(s["items"][1]["name"], "host pi held");
        assert_eq!(s["items"][1]["label"], "2d, held");
        assert_eq!(s["items"][1]["held_reason"], "linux fails");
        assert_eq!(s["items"][0]["target"], "alice");
        assert_eq!(s["items"][0]["target_kind"], "home");
        assert_eq!(s["items"][0]["inputs"][0], "nixpkgs");
        assert_eq!(s["items"][0]["attr"], "zed-editor");
    }

    #[test]
    fn manual_pins_never_go_stale() {
        let mut f = PinsFile::default();
        let mut p = pin("2026-01-01");
        p.reason = Reason::Manual;
        f.entry_mut(Kind::Nixos, "laptop")
            .insert("nixpkgs", "hello", p);
        let s = summary(&f, today(), 14);
        assert_eq!(s["status"], "PINNED");
        assert_eq!(s["stale"], 0);
        assert_eq!(s["items"][0]["label"], "299d, manual");
    }
}
