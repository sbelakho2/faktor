//! Strict dot-separated numeric versions and closed ranges.
//!
//! The updater deliberately does not accept prerelease/build metadata or
//! semver operator syntax: a compatibility range in a SIGNED manifest must
//! be unambiguous, so only `MAJOR[.MINOR[.PATCH[.BUILD]]]` numeric fields are
//! parsed (a leading `v` is tolerated). `*` is the only wildcard and means
//! "any version" — it is legal only as `min` or `max` of a range.

use std::fmt;

/// One parsed numeric version (at most four fields, each `<= u32::MAX`).
///
/// Ordering, equality and hashing are over the NUMERIC fields only, so
/// `v1.2.3` and `1.2.3` are the same version (the raw spelling is preserved
/// for display).
#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    fields: [u64; 4],
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
    }
}

impl Eq for Version {}

impl std::hash::Hash for Version {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.fields.hash(state);
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fields.cmp(&other.fields)
    }
}

impl Version {
    /// Parse a version strictly. `1`, `1.2`, `1.2.3`, `v1.2.3.4` are valid;
    /// empty fields, leading zeros, separators only, and more than four
    /// fields are refused.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.strip_prefix('v').unwrap_or(raw);
        if raw.is_empty() || raw.len() > 64 {
            return Err(format!("version {raw:?} must be 1..=64 bytes"));
        }
        if !trimmed.is_ascii() {
            return Err(format!("version {raw:?} must be ASCII"));
        }
        let parts: Vec<&str> = trimmed.split('.').collect();
        if parts.is_empty() || parts.len() > 4 {
            return Err(format!(
                "version {raw:?} must have 1..=4 dot-separated numeric fields"
            ));
        }
        let mut fields = [0u64; 4];
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("version {raw:?} has a non-numeric field {part:?}"));
            }
            if part.len() > 1 && part.starts_with('0') {
                return Err(format!("version {raw:?} has a leading-zero field {part:?}"));
            }
            let value: u64 = part
                .parse()
                .map_err(|_| format!("version field {part:?} overflows"))?;
            if value > u32::MAX as u64 {
                return Err(format!("version field {part:?} exceeds u32::MAX"));
            }
            fields[i] = value;
        }
        Ok(Version {
            raw: raw.to_string(),
            fields,
        })
    }

    /// The version exactly as it appeared in the manifest.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// One closed compatibility range: `min`/`max` are INCLUSIVE; either end may
/// be `*` (unbounded). `min > max` is refused at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange {
    min: Option<Version>,
    max: Option<Version>,
}

impl VersionRange {
    pub fn new(min: Option<Version>, max: Option<Version>) -> Result<Self, String> {
        if let (Some(min), Some(max)) = (&min, &max) {
            if min > max {
                return Err(format!("range min {min} exceeds max {max}"));
            }
        }
        Ok(VersionRange { min, max })
    }

    /// Parse the manifest wire shape of one range.
    pub fn from_parts(min: &str, max: &str) -> Result<Self, String> {
        let min = parse_bound(min, "min")?;
        let max = parse_bound(max, "max")?;
        Self::new(min, max)
    }

    pub fn contains(&self, version: &Version) -> bool {
        if let Some(min) = &self.min {
            if version < min {
                return false;
            }
        }
        if let Some(max) = &self.max {
            if version > max {
                return false;
            }
        }
        true
    }

    /// True when this range accepts every version (`*`..`*`).
    pub fn is_unbounded(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    pub fn render(&self) -> String {
        let min = self.min.as_ref().map_or("*".to_string(), |v| v.to_string());
        let max = self.max.as_ref().map_or("*".to_string(), |v| v.to_string());
        format!("{min}..{max}")
    }
}

fn parse_bound(raw: &str, which: &str) -> Result<Option<Version>, String> {
    if raw == "*" {
        return Ok(None);
    }
    Version::parse(raw)
        .map(Some)
        .map_err(|e| format!("range {which}: {e}"))
}

/// The strict manifest wire shape `{"min": "...", "max": "..."}`: exactly
/// these two keys, string values, `*` the only wildcard, unknown/duplicate
/// keys and inverted ranges refused.
impl serde::Serialize for VersionRange {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let render = |bound: &Option<Version>| {
            bound
                .as_ref()
                .map_or_else(|| "*".to_string(), |v| v.to_string())
        };
        let mut state = serializer.serialize_struct("VersionRange", 2)?;
        state.serialize_field("min", &render(&self.min))?;
        state.serialize_field("max", &render(&self.max))?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for VersionRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct RangeVisitor;

        impl<'de> Visitor<'de> for RangeVisitor {
            type Value = VersionRange;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a version range object {min, max}")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<VersionRange, A::Error> {
                let mut min: Option<Option<Version>> = None;
                let mut max: Option<Option<Version>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "min" => {
                            if min.is_some() {
                                return Err(A::Error::duplicate_field("min"));
                            }
                            let raw = map.next_value::<String>()?;
                            min = Some(parse_bound(&raw, "min").map_err(A::Error::custom)?);
                        }
                        "max" => {
                            if max.is_some() {
                                return Err(A::Error::duplicate_field("max"));
                            }
                            let raw = map.next_value::<String>()?;
                            max = Some(parse_bound(&raw, "max").map_err(A::Error::custom)?);
                        }
                        other => return Err(A::Error::unknown_field(other, &["min", "max"])),
                    }
                }
                let min = min.ok_or_else(|| A::Error::missing_field("min"))?;
                let max = max.ok_or_else(|| A::Error::missing_field("max"))?;
                VersionRange::new(min, max).map_err(A::Error::custom)
            }
        }

        deserializer.deserialize_map(RangeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        Version::parse(raw).unwrap()
    }

    #[test]
    fn versions_parse_strictly_and_compare_numerically() {
        assert!(v("1.2.3") < v("1.2.10"));
        assert!(v("1.2") < v("1.2.1"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert!(v("2") > v("1.999.999"));
        for bad in [
            "",
            ".",
            "..",
            "1.",
            ".1",
            "1..2",
            "1.2.3.4.5",
            "01.2",
            "1.a",
            "1.2.-3",
            "1.2.3 4",
            "1.2.3+meta",
            "1.2.3-rc1",
            "9999999999",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn ranges_are_closed_and_wildcards_are_explicit() {
        let range = VersionRange::from_parts("1.0.0", "2.0.0").unwrap();
        assert!(range.contains(&v("1.0.0")));
        assert!(range.contains(&v("2.0.0")));
        assert!(!range.contains(&v("0.9.9")));
        assert!(!range.contains(&v("2.0.1")));
        assert!(!range.is_unbounded());

        let open = VersionRange::from_parts("*", "*").unwrap();
        assert!(open.is_unbounded());
        assert!(open.contains(&v("0.0.1")));

        let from = VersionRange::from_parts("1.5.0", "*").unwrap();
        assert!(!from.contains(&v("1.4.9")));
        assert!(from.contains(&v("99.0.0")));

        assert!(VersionRange::from_parts("2.0.0", "1.0.0").is_err());
        assert!(VersionRange::from_parts("1.0.0", "banana").is_err());
    }
}
