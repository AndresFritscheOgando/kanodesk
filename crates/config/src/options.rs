//! The option table (12 §6): every option has one typed definition here —
//! key, scope, type, default, settings section, description and whether a
//! policy may fix it. Validation, defaults and (later) the settings UI all
//! come from this table.

/// Who owns an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Host-wide; owned by the host (server) process, changed through it.
    Host,
    /// This user's viewer/UI only.
    Local,
}

/// An option's type and allowed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `"Y"` / `"N"`.
    Bool,
    /// A whole number in `min..=max`.
    Int {
        /// Smallest allowed value.
        min: i64,
        /// Largest allowed value.
        max: i64,
    },
    /// One of a fixed set of words.
    Choice(&'static [&'static str]),
    /// Any text (validated elsewhere, e.g. addresses).
    Text,
}

/// One option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionDef {
    /// Key in the config file (kebab-case, RustDesk's names where they exist).
    pub key: &'static str,
    /// Owner.
    pub scope: Scope,
    /// Type and allowed values.
    pub kind: Kind,
    /// Default value, in the stored form.
    pub default: &'static str,
    /// Settings section it belongs to.
    pub section: &'static str,
    /// One-line description for the settings UI.
    pub description: &'static str,
    /// Whether an administrator policy may fix the value.
    pub lockable: bool,
}

impl OptionDef {
    /// Checks a value against the option's type; returns its normal form.
    pub fn normalise(&self, value: &str) -> Option<String> {
        let v = value.trim();
        match self.kind {
            Kind::Bool => match v.to_ascii_lowercase().as_str() {
                "y" | "yes" | "true" | "1" | "on" => Some("Y".into()),
                "n" | "no" | "false" | "0" | "off" | "" => Some("N".into()),
                _ => None,
            },
            Kind::Int { min, max } => v
                .parse::<i64>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .map(|n| n.to_string()),
            Kind::Choice(words) => words
                .iter()
                .find(|w| w.eq_ignore_ascii_case(v))
                .map(|w| (*w).to_owned()),
            Kind::Text => Some(v.to_owned()),
        }
    }
}

/// How incoming sessions are approved.
pub const APPROVE_MODES: &[&str] = &["password", "click", "both"];

/// Every option.
pub const OPTIONS: &[OptionDef] = &[
    // Security and access.
    OptionDef {
        key: "approve-mode",
        scope: Scope::Host,
        kind: Kind::Choice(APPROVE_MODES),
        default: "password",
        section: "security",
        description: "How incoming sessions are approved: password only, a click in the connection manager only, or both",
        lockable: true,
    },
    OptionDef {
        key: "temporary-password-length",
        scope: Scope::Host,
        kind: Kind::Int { min: 6, max: 32 },
        default: "8",
        section: "security",
        description: "Length of the one-time temporary password",
        lockable: true,
    },
    OptionDef {
        key: "approve-click-delay-ms",
        scope: Scope::Host,
        kind: Kind::Int {
            min: 0,
            max: 10_000,
        },
        default: "1000",
        section: "security",
        description: "How long the Accept button stays disabled after a request appears (stops a remote user from clicking it)",
        lockable: true,
    },
    OptionDef {
        key: "approve-timeout-secs",
        scope: Scope::Host,
        kind: Kind::Int { min: 5, max: 600 },
        default: "60",
        section: "security",
        description: "How long a viewer waits for the host's user to accept before it is refused",
        lockable: true,
    },
    // Default permissions of incoming sessions.
    OptionDef {
        key: "enable-keyboard",
        scope: Scope::Host,
        kind: Kind::Bool,
        default: "Y",
        section: "permissions",
        description: "Let viewers use the keyboard",
        lockable: true,
    },
    OptionDef {
        key: "enable-mouse",
        scope: Scope::Host,
        kind: Kind::Bool,
        default: "Y",
        section: "permissions",
        description: "Let viewers use the mouse",
        lockable: true,
    },
    // Network.
    OptionDef {
        key: "direct-server",
        scope: Scope::Host,
        kind: Kind::Bool,
        default: "Y",
        section: "network",
        description: "Accept connections directly on a local port (LAN or port forwarding), without the ID server",
        lockable: true,
    },
    OptionDef {
        key: "direct-access-listen",
        scope: Scope::Host,
        kind: Kind::Text,
        default: "0.0.0.0:21118",
        section: "network",
        description: "Address and UDP port for direct connections",
        lockable: true,
    },
    OptionDef {
        key: "custom-rendezvous-server",
        scope: Scope::Host,
        kind: Kind::Text,
        default: "",
        section: "network",
        description: "ID server host:port",
        lockable: true,
    },
    OptionDef {
        key: "key",
        scope: Scope::Host,
        kind: Kind::Text,
        default: "",
        section: "network",
        description: "The ID server's public key (hex)",
        lockable: true,
    },
    // Video.
    OptionDef {
        key: "custom-fps",
        scope: Scope::Host,
        kind: Kind::Int { min: 1, max: 240 },
        default: "30",
        section: "video",
        description: "Frames per second sent to viewers",
        lockable: false,
    },
    OptionDef {
        key: "video-bitrate-kbps",
        scope: Scope::Host,
        kind: Kind::Int {
            min: 0,
            max: 200_000,
        },
        default: "0",
        section: "video",
        description: "Video bitrate in kbit/s (0 = automatic for the display size)",
        lockable: false,
    },
    // UI.
    OptionDef {
        key: "hide_cm",
        scope: Scope::Host,
        kind: Kind::Bool,
        default: "N",
        section: "ui",
        description: "Do not show the connection manager window (unattended hosts; needs the password-only approve mode)",
        lockable: true,
    },
];

/// The definition of `key`, if it is a known option.
pub fn find(key: &str) -> Option<&'static OptionDef> {
    OPTIONS.iter().find(|o| o.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_consistent() {
        let mut keys = HashSet::new();
        for o in OPTIONS {
            assert!(keys.insert(o.key), "duplicate {}", o.key);
            assert_eq!(
                o.normalise(o.default).as_deref(),
                Some(o.default),
                "default of {} is not in normal form",
                o.key
            );
            assert!(!o.description.is_empty() && !o.section.is_empty());
        }
    }

    #[test]
    fn values_are_normalised_or_refused() {
        let b = find("enable-keyboard").unwrap();
        assert_eq!(b.normalise("yes").as_deref(), Some("Y"));
        assert_eq!(b.normalise("OFF").as_deref(), Some("N"));
        assert_eq!(b.normalise("maybe"), None);
        let i = find("custom-fps").unwrap();
        assert_eq!(i.normalise(" 60 ").as_deref(), Some("60"));
        assert_eq!(i.normalise("0"), None);
        assert_eq!(i.normalise("x"), None);
        let c = find("approve-mode").unwrap();
        assert_eq!(c.normalise("Click").as_deref(), Some("click"));
        assert_eq!(c.normalise("never"), None);
        assert!(find("no-such-option").is_none());
    }
}
