//! Compatibility policy: every component the manifest declares a range for
//! is checked against the RUNNING component version and reported by name.
//!
//! Rules (no silent skips):
//!
//! - a known running version outside its range is `OutOfRange` and refuses;
//! - an UNKNOWN running version (the component is not present/attached) is
//!   `Unknown` and refuses whenever the range is bounded — a bounded range
//!   is an assertion about a component this update expects to find, so
//!   "can't tell" is not "compatible";
//! - only an unbounded (`*`..`*`) range tolerates an unknown running
//!   version, because it asserts nothing.
//!
//! The report names every component (cli/daemon/vscode/jetbrains/schema)
//! with its running version, range, and verdict, so a refusal is actionable
//! without log archaeology.

use crate::manifest::Compatibility;
use crate::version::{Version, VersionRange};

/// The components the manifest constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component {
    Cli,
    Daemon,
    Vscode,
    Jetbrains,
    Schema,
}

impl Component {
    pub const ALL: &'static [Component] = &[
        Component::Cli,
        Component::Daemon,
        Component::Vscode,
        Component::Jetbrains,
        Component::Schema,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Component::Cli => "cli",
            Component::Daemon => "daemon",
            Component::Vscode => "vscode",
            Component::Jetbrains => "jetbrains",
            Component::Schema => "schema",
        }
    }
}

/// One component's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Running version known and inside the range.
    Ok,
    /// Running version known and outside the range.
    OutOfRange { running: String },
    /// Running version unknown and the range is bounded.
    Unknown,
    /// Running version unknown and the range is unbounded (`*`..`*`).
    Unconstrained,
}

impl Verdict {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::OutOfRange { .. } => "out_of_range",
            Verdict::Unknown => "unknown",
            Verdict::Unconstrained => "unconstrained",
        }
    }

    pub const fn is_refusal(&self) -> bool {
        matches!(self, Verdict::OutOfRange { .. } | Verdict::Unknown)
    }
}

/// One component's full verdict row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentVerdict {
    pub component: Component,
    /// The running version, when known.
    pub running: Option<String>,
    /// The manifest's declared range, rendered (`"1.0.0..2.0.0"`, `"*..*"`).
    pub range: String,
    pub verdict: Verdict,
}

/// The full compatibility report. `refused` is true when any component row
/// refuses; the rows are always all present, so a caller can report every
/// component even when the first one already failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityReport {
    pub rows: Vec<ComponentVerdict>,
    pub refused: bool,
}

impl std::fmt::Display for CompatibilityReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for row in &self.rows {
            if !row.verdict.is_refusal() {
                continue;
            }
            if !first {
                f.write_str("; ")?;
            }
            first = false;
            match &row.verdict {
                Verdict::OutOfRange { running } => write!(
                    f,
                    "{} running {} is outside {}",
                    row.component.as_str(),
                    running,
                    row.range
                )?,
                Verdict::Unknown => write!(
                    f,
                    "{} running version is unknown but the update requires {}",
                    row.component.as_str(),
                    row.range
                )?,
                _ => {}
            }
        }
        if first {
            f.write_str("all components compatible")?;
        }
        Ok(())
    }
}

impl CompatibilityReport {
    /// The components that refused, by name.
    pub fn refused_components(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|row| row.verdict.is_refusal())
            .map(|row| row.component.as_str())
            .collect()
    }

    pub fn is_compatible(&self) -> bool {
        !self.refused
    }
}

/// The running versions of the components on this host. `None` = the
/// component is not present/attached (never guessed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunningComponents {
    pub cli: Option<Version>,
    pub daemon: Option<Version>,
    pub vscode: Option<Version>,
    pub jetbrains: Option<Version>,
    /// The native protocol schema version the running daemon speaks.
    pub schema: Option<u32>,
}

impl RunningComponents {
    pub fn from_strs(
        cli: Option<&str>,
        daemon: Option<&str>,
        vscode: Option<&str>,
        jetbrains: Option<&str>,
        schema: Option<u32>,
    ) -> Result<Self, String> {
        Ok(RunningComponents {
            cli: parse_opt(cli)?,
            daemon: parse_opt(daemon)?,
            vscode: parse_opt(vscode)?,
            jetbrains: parse_opt(jetbrains)?,
            schema,
        })
    }
}

fn parse_opt(raw: Option<&str>) -> Result<Option<Version>, String> {
    match raw {
        None => Ok(None),
        Some(raw) => Version::parse(raw).map(Some),
    }
}

/// Check the manifest's compatibility block against the running components.
pub fn check(compatibility: &Compatibility, running: &RunningComponents) -> CompatibilityReport {
    let mut rows = Vec::with_capacity(Component::ALL.len());
    for &component in Component::ALL {
        let (range, known): (&dyn RangeProbe, bool) = match component {
            Component::Cli => (&compatibility.cli, running.cli.is_some()),
            Component::Daemon => (&compatibility.daemon, running.daemon.is_some()),
            Component::Vscode => (&compatibility.vscode, running.vscode.is_some()),
            Component::Jetbrains => (&compatibility.jetbrains, running.jetbrains.is_some()),
            Component::Schema => (&compatibility.schema, running.schema.is_some()),
        };
        let verdict = if !known {
            if range.unbounded() {
                Verdict::Unconstrained
            } else {
                Verdict::Unknown
            }
        } else if range.satisfied_by(component, running) {
            Verdict::Ok
        } else {
            Verdict::OutOfRange {
                running: range.running_render(component, running),
            }
        };
        rows.push(ComponentVerdict {
            component,
            running: if known {
                Some(range.running_render(component, running))
            } else {
                None
            },
            range: range.render(),
            verdict,
        });
    }
    let refused = rows.iter().any(|row| row.verdict.is_refusal());
    CompatibilityReport { rows, refused }
}

/// Version ranges and schema ranges share one verdict pipeline.
trait RangeProbe {
    fn unbounded(&self) -> bool;
    fn satisfied_by(&self, component: Component, running: &RunningComponents) -> bool;
    fn running_render(&self, component: Component, running: &RunningComponents) -> String;
    fn render(&self) -> String;
}

impl RangeProbe for VersionRange {
    fn unbounded(&self) -> bool {
        self.is_unbounded()
    }

    fn satisfied_by(&self, component: Component, running: &RunningComponents) -> bool {
        let version = match component {
            Component::Cli => &running.cli,
            Component::Daemon => &running.daemon,
            Component::Vscode => &running.vscode,
            Component::Jetbrains => &running.jetbrains,
            Component::Schema => return true,
        };
        version
            .as_ref()
            .is_some_and(|version| self.contains(version))
    }

    fn running_render(&self, component: Component, running: &RunningComponents) -> String {
        match component {
            Component::Cli => render_opt(&running.cli),
            Component::Daemon => render_opt(&running.daemon),
            Component::Vscode => render_opt(&running.vscode),
            Component::Jetbrains => render_opt(&running.jetbrains),
            Component::Schema => render_opt(&running.cli),
        }
    }

    fn render(&self) -> String {
        VersionRange::render(self)
    }
}

/// The manifest's `schema` entry is a numeric range over the native protocol
/// schema version (the wire schema the update expects). Wire shape:
/// `{"min": <u32>, "max": <u32>}` with `min <= max`; unknown and duplicate
/// keys are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaRange {
    pub min: u32,
    pub max: u32,
}

impl SchemaRange {
    pub fn new(min: u32, max: u32) -> Result<Self, String> {
        if min > max {
            return Err(format!("schema range min {min} exceeds max {max}"));
        }
        Ok(SchemaRange { min, max })
    }
}

impl serde::Serialize for SchemaRange {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SchemaRange", 2)?;
        state.serialize_field("min", &self.min)?;
        state.serialize_field("max", &self.max)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for SchemaRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SchemaRangeVisitor;

        impl<'de> Visitor<'de> for SchemaRangeVisitor {
            type Value = SchemaRange;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a schema range object {min, max}")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SchemaRange, A::Error> {
                let mut min: Option<u32> = None;
                let mut max: Option<u32> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "min" => {
                            if min.is_some() {
                                return Err(A::Error::duplicate_field("min"));
                            }
                            min = Some(map.next_value::<u32>()?);
                        }
                        "max" => {
                            if max.is_some() {
                                return Err(A::Error::duplicate_field("max"));
                            }
                            max = Some(map.next_value::<u32>()?);
                        }
                        other => return Err(A::Error::unknown_field(other, &["min", "max"])),
                    }
                }
                let min = min.ok_or_else(|| A::Error::missing_field("min"))?;
                let max = max.ok_or_else(|| A::Error::missing_field("max"))?;
                SchemaRange::new(min, max).map_err(A::Error::custom)
            }
        }

        deserializer.deserialize_map(SchemaRangeVisitor)
    }
}

impl RangeProbe for SchemaRange {
    fn unbounded(&self) -> bool {
        false
    }

    fn satisfied_by(&self, _component: Component, running: &RunningComponents) -> bool {
        running
            .schema
            .is_some_and(|schema| schema >= self.min && schema <= self.max)
    }

    fn running_render(&self, _component: Component, running: &RunningComponents) -> String {
        match running.schema {
            Some(schema) => schema.to_string(),
            None => "unknown".to_string(),
        }
    }

    fn render(&self) -> String {
        format!("{}..{}", self.min, self.max)
    }
}

fn render_opt(version: &Option<Version>) -> String {
    version
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Compatibility;

    fn compat(min: &str, max: &str) -> Compatibility {
        Compatibility {
            cli: VersionRange::from_parts(min, max).unwrap(),
            daemon: VersionRange::from_parts(min, max).unwrap(),
            vscode: VersionRange::from_parts("*", "*").unwrap(),
            jetbrains: VersionRange::from_parts("*", "*").unwrap(),
            schema: SchemaRange { min: 1, max: 1 },
        }
    }

    fn running(cli: &str, daemon: &str) -> RunningComponents {
        RunningComponents {
            cli: Version::parse(cli).ok(),
            daemon: Version::parse(daemon).ok(),
            vscode: None,
            jetbrains: None,
            schema: Some(1),
        }
    }

    #[test]
    fn compatible_components_pass_and_report_every_row() {
        let report = check(&compat("1.0.0", "2.0.0"), &running("1.5.0", "1.9.9"));
        assert!(report.is_compatible());
        assert_eq!(report.rows.len(), Component::ALL.len());
        assert_eq!(report.rows[0].component, Component::Cli);
        assert_eq!(report.rows[0].verdict, Verdict::Ok);
        assert_eq!(report.rows[2].verdict, Verdict::Unconstrained);
    }

    #[test]
    fn out_of_range_components_refuse_and_are_named() {
        let report = check(&compat("1.0.0", "1.9.0"), &running("2.0.0", "0.1.0"));
        assert!(report.refused);
        assert_eq!(report.refused_components(), vec!["cli", "daemon"]);
        let rendered = report.to_string();
        assert!(
            rendered.contains("cli running 2.0.0 is outside 1.0.0..1.9.0"),
            "{rendered}"
        );
        assert!(rendered.contains("daemon running 0.1.0"), "{rendered}");
    }

    #[test]
    fn a_bounded_range_with_an_unknown_component_refuses() {
        let mut compatibility = compat("1.0.0", "2.0.0");
        compatibility.vscode = VersionRange::from_parts("1.0.0", "2.0.0").unwrap();
        let report = check(&compatibility, &running("1.5.0", "1.5.0"));
        assert!(report.refused);
        assert_eq!(report.refused_components(), vec!["vscode"]);
        assert!(report
            .to_string()
            .contains("vscode running version is unknown"));
    }

    #[test]
    fn an_unbounded_range_tolerates_an_unknown_component() {
        let report = check(&compat("1.0.0", "2.0.0"), &running("1.5.0", "1.5.0"));
        assert!(report.is_compatible());
    }

    #[test]
    fn the_schema_range_is_checked_and_named() {
        let mut compatibility = compat("1.0.0", "2.0.0");
        compatibility.schema = SchemaRange { min: 2, max: 2 };
        let report = check(&compatibility, &running("1.5.0", "1.5.0"));
        assert_eq!(report.refused_components(), vec!["schema"]);
        assert!(report
            .to_string()
            .contains("schema running 1 is outside 2..2"));
    }
}
