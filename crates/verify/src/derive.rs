//! @authority-collapse: authority-bearing
//!
//! Multi-component project profiles (audit: single-project verification).
//!
//! A repository is not one project. Detection returns EVERY project
//! component (root, languages, build systems, toolchains, targets, test
//! frameworks), ordered most-specific root first, and derivation maps every
//! changed file to its owning component by longest-root prefix — never to a
//! first-match project type that certifies the wrong slice of a mixed repo.
//! Caps (checks, total wall budget, simultaneous background jobs) are typed
//! refusals, never silent truncation of required semantic coverage.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Test-only per-thread probe counter for the profile lookups (evidence for
// the complexity tests): one unit per candidate probe the ancestor walks
// make. Production builds carry no counter and no branches into one.
#[cfg(test)]
thread_local! {
    pub(crate) static PROFILE_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[inline(always)]
fn count_probe() {
    #[cfg(test)]
    PROFILE_PROBES.with(|counter| counter.set(counter.get() + 1));
}

use crate::exec::{CheckCategory, CheckKind, CheckSpec, BUILD_DIR};
use crate::inventory::{
    probe_manifest_text, CandidateGeneration, ManifestProbe, ManifestProbeRefusal,
};
use crate::{
    compileall_token, is_test_related, rust_test_filter, under_dir, MAX_CHECKS,
    MAX_SIMULTANEOUS_JOBS, MAX_TOTAL_WALL_BUDGET,
};

/// The language families a component may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LanguageFamily {
    Rust,
    Node,
    Python,
    Go,
    Java,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Assembly,
}

/// The build systems a component may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuildSystem {
    Cargo,
    Npm,
    Maven,
    Gradle,
    CMake,
    Make,
    Ninja,
    Meson,
    Bazel,
    MSBuild,
    DotNet,
    PlatformIO,
    ZephyrWest,
    EspIdf,
}

/// The toolchains a component's checks require. Availability is decided by
/// the executor (`Unavailable` when missing), never guessed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Toolchain {
    Rust,
    Node,
    Python,
    Go,
    Jdk,
    DotNet,
    CMake,
    Make,
    Ninja,
    Meson,
    Bazel,
    MSBuild,
    Gradle,
    PlatformIO,
    West,
    ZephyrSdk,
    ArmNoneEabi,
    EspXtensa,
}

/// The target class a component builds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetProfile {
    Host,
    ArmNoneEabi,
    Esp32,
    Zephyr,
}

/// The test frameworks a component's checks recognize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TestFramework {
    CargoTest,
    Pytest,
    GoTest,
    JUnit,
    Jest,
    Vitest,
    Mocha,
    CTest,
    GoogleTest,
    UnityCMock,
    ZephyrTwister,
    PlatformIoTest,
    DotNetTest,
    XUnit,
    NUnit,
    MakeTest,
    MakeCheck,
}

/// One component of a repository: a project rooted at `root` (relative,
/// `/`-separated, empty for the repository root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectComponent {
    pub root: PathBuf,
    pub languages: Vec<LanguageFamily>,
    pub build_systems: Vec<BuildSystem>,
    pub toolchains: Vec<Toolchain>,
    pub targets: Vec<TargetProfile>,
    pub test_frameworks: Vec<TestFramework>,
}

/// Every project component of a repository, most-specific root first, plus
/// the typed refusals of every content-deciding manifest probe that could
/// not certify its content (unreadable/oversized/unstable). A refusal NEVER
/// acts as absence: [`derive_checks`] refuses a profile carrying one, so a
/// `Passed` verdict can never rest on silently-dropped manifest content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectProfile {
    pub components: Vec<ProjectComponent>,
    pub manifest_refusals: Vec<ManifestProbeRefusal>,
}

impl ProjectProfile {
    /// The component owning `file`: the longest root that is a path prefix.
    /// Deterministic (first component wins only on an impossible tie).
    pub fn owning_component(&self, file: &Path) -> Option<&ProjectComponent> {
        let file = file.to_string_lossy();
        let mut best: Option<(usize, &ProjectComponent)> = None;
        for component in &self.components {
            let root = component.root.to_string_lossy();
            if !is_under(&root, &file) {
                continue;
            }
            let depth = root.matches('/').count() + usize::from(!root.is_empty());
            if best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, component));
            }
        }
        best.map(|(_, component)| component)
    }

    /// Owner indices for MANY files with exactly the semantics of
    /// [`ProjectProfile::owning_component`] (longest root prefix; the first
    /// component wins a tie), computed with ONE lookup-table build and a
    /// per-file string-ancestor walk — O(components + files · depth)
    /// instead of a full component scan per file. The walk enumerates the
    /// same candidate roots the scan's `is_under` accepts (`file` itself,
    /// then each `/`-boundary prefix down to `""`), byte-identical for any
    /// input the scan accepts.
    pub fn owner_indices<'f>(
        &self,
        files: impl IntoIterator<Item = &'f Path>,
    ) -> Vec<Option<usize>> {
        let mut by_root: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(self.components.len());
        for (index, component) in self.components.iter().enumerate() {
            by_root
                .entry(component.root.to_string_lossy().into_owned())
                .or_insert(index);
        }
        files
            .into_iter()
            .map(|file| {
                let file = file.to_string_lossy();
                let mut candidate: &str = &file;
                loop {
                    count_probe();
                    if let Some(&index) = by_root.get(candidate) {
                        return Some(index);
                    }
                    match candidate.rsplit_once('/') {
                        Some((head, _)) => candidate = head,
                        None if !candidate.is_empty() => candidate = "",
                        None => return None,
                    }
                }
            })
            .collect()
    }
}

/// A typed refusal to derive: exceeding a stated cap is NEVER silent
/// truncation of required semantic coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationDerivationError {
    TooManyChecks {
        count: usize,
        max: usize,
    },
    TotalWallBudgetExceeded {
        estimated: Duration,
        max: Duration,
    },
    TooManySimultaneousJobs {
        jobs: usize,
        max: usize,
    },
    ConflictingCheckId {
        id: String,
    },
    /// A content-deciding manifest probe could not certify its content
    /// (unreadable/oversized/unstable): the derived check set would be
    /// silently incomplete, so derivation refuses typed.
    ManifestProbeRefused {
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for VerificationDerivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyChecks { count, max } => {
                write!(f, "derived {count} checks, over the maximum of {max}")
            }
            Self::TotalWallBudgetExceeded { estimated, max } => write!(
                f,
                "derived checks need {}s of wall budget, over the maximum of {}s",
                estimated.as_secs(),
                max.as_secs()
            ),
            Self::TooManySimultaneousJobs { jobs, max } => write!(
                f,
                "derived {jobs} background jobs, over the maximum of {max} simultaneous jobs"
            ),
            Self::ConflictingCheckId { id } => {
                write!(f, "two different checks claim the id {id:?}")
            }
            Self::ManifestProbeRefused { path, detail } => {
                write!(
                    f,
                    "manifest {path:?} could not certify its content: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for VerificationDerivationError {}

/// The derivation caps. Exceeding any of them is a typed error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivationLimits {
    pub max_checks: usize,
    pub max_total_wall: Duration,
    pub max_simultaneous_jobs: usize,
}

impl Default for DerivationLimits {
    fn default() -> Self {
        Self {
            max_checks: MAX_CHECKS,
            max_total_wall: MAX_TOTAL_WALL_BUDGET,
            max_simultaneous_jobs: MAX_SIMULTANEOUS_JOBS,
        }
    }
}

/// Bounded content-probe cap of one manifest: a larger manifest's content
/// is never read, so content-derived evidence is silently unavailable. The
/// inventory surfaces oversized CONTENT-DECIDING manifests
/// (`crates/verify/src/inventory.rs`, `ManifestOversized`) against this cap.
pub(crate) const MAX_PROBE_BYTES: u64 = 256 * 1024;
const NOMINAL_QUICK: Duration = Duration::from_secs(60);
const NOMINAL_UNIT: Duration = Duration::from_secs(600);

fn nominal_budget(category: CheckCategory) -> Duration {
    match category {
        CheckCategory::Quick => NOMINAL_QUICK,
        CheckCategory::Unit | CheckCategory::Full => NOMINAL_UNIT,
    }
}

fn normalize_rel(path: &str) -> Option<String> {
    if path.is_empty() || path.starts_with('/') {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other => segments.push(other),
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn is_under(root: &str, path: &str) -> bool {
    if root.is_empty() {
        return true;
    }
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn relative_to<'a>(root: &str, file: &'a str) -> Option<&'a str> {
    if root.is_empty() {
        Some(file)
    } else {
        file.strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
    }
}

pub(crate) fn component_id(component: &ProjectComponent, base: &str) -> String {
    let root = component.root.to_string_lossy();
    if root.is_empty() {
        return base.to_string();
    }
    let sanitized: String = root
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}:{base}")
}

/// Read one manifest's content through the generation-bound
/// [`probe_manifest_text`]. A content-deciding manifest that is not
/// `Present` records its typed refusal (never a silent absence); `Absent`
/// records nothing (there is genuinely no manifest to read).
fn read_manifest(
    root: &Path,
    generation: &CandidateGeneration,
    rel: &str,
    refusals: &mut Vec<ManifestProbeRefusal>,
) -> Option<String> {
    match probe_manifest_text(root, generation, rel) {
        ManifestProbe::Present(text) => Some(text),
        ManifestProbe::Absent => None,
        refusal => {
            if let Some(refusal) = refusal.refusal() {
                if !refusals.iter().any(|seen| seen.path == refusal.path) {
                    refusals.push(refusal);
                }
            }
            None
        }
    }
}

fn probe_any(
    root: &Path,
    generation: &CandidateGeneration,
    rel: &str,
    needles: &[&str],
    refusals: &mut Vec<ManifestProbeRefusal>,
) -> bool {
    read_manifest(root, generation, rel, refusals).is_some_and(|text| {
        let text = text.to_ascii_lowercase();
        needles.iter().any(|needle| text.contains(needle))
    })
}

fn component_at<'a>(
    components: &'a mut BTreeMap<String, ProjectComponent>,
    dir: &str,
) -> &'a mut ProjectComponent {
    // The lookup-first form never allocates the key on a hit (the entry API
    // would reconstruct `dir.to_string()` on every call, hit or miss).
    if components.get(dir).is_none() {
        components.insert(
            dir.to_string(),
            ProjectComponent {
                root: PathBuf::from(dir),
                languages: Vec::new(),
                build_systems: Vec::new(),
                toolchains: Vec::new(),
                targets: Vec::new(),
                test_frameworks: Vec::new(),
            },
        );
    }
    components
        .get_mut(dir)
        .expect("the component was just inserted when absent")
}

fn nearest_key(components: &BTreeMap<String, ProjectComponent>, dir: &str) -> Option<String> {
    // Ancestor walk (deepest first): every component root that `is_under`
    // accepts for `dir` is an ancestor prefix of `dir`, and ancestors are
    // totally ordered by depth, so the first hit is exactly the deepest
    // (max-depth) one the old full scan selected. O(depth · log m) probes
    // instead of O(m).
    let mut candidate = dir;
    loop {
        count_probe();
        if components.contains_key(candidate) {
            return Some(candidate.to_string());
        }
        match candidate.rsplit_once('/') {
            Some((parent, _)) => candidate = parent,
            None if !candidate.is_empty() => candidate = "",
            None => return None,
        }
    }
}

fn add_language(component: &mut ProjectComponent, language: LanguageFamily) {
    if !component.languages.contains(&language) {
        component.languages.push(language);
    }
}

fn add_build_system(component: &mut ProjectComponent, build_system: BuildSystem) {
    if !component.build_systems.contains(&build_system) {
        component.build_systems.push(build_system);
    }
}

fn add_toolchain(component: &mut ProjectComponent, toolchain: Toolchain) {
    if !component.toolchains.contains(&toolchain) {
        component.toolchains.push(toolchain);
    }
}

fn add_target(component: &mut ProjectComponent, target: TargetProfile) {
    if !component.targets.contains(&target) {
        component.targets.push(target);
    }
}

fn add_test_framework(component: &mut ProjectComponent, framework: TestFramework) {
    if !component.test_frameworks.contains(&framework) {
        component.test_frameworks.push(framework);
    }
}

fn probe_make_targets(
    root: &Path,
    generation: &CandidateGeneration,
    dir: &str,
    files: &[String],
    refusals: &mut Vec<ManifestProbeRefusal>,
) -> (bool, bool) {
    let Some(rel) = ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .map(|name| join_rel(dir, name))
        .find(|rel| files.binary_search(rel).is_ok())
    else {
        return (false, false);
    };
    let Some(text) = read_manifest(root, generation, &rel, refusals) else {
        return (false, false);
    };
    let (mut test, mut check) = (false, false);
    for line in text.lines().take(4096) {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        if let Some((target, _)) = line.split_once(':') {
            match target.trim() {
                "test" => test = true,
                "check" => check = true,
                _ => {}
            }
        }
    }
    (test, check)
}

fn detect_marker(
    root: &Path,
    generation: &CandidateGeneration,
    components: &mut BTreeMap<String, ProjectComponent>,
    files: &[String],
    rel: &str,
    refusals: &mut Vec<ManifestProbeRefusal>,
) {
    let dir = dir_of(rel).to_string();
    let name = basename(rel);
    let gradlew = join_rel(&dir, "gradlew");
    if name == "Cargo.toml" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Rust);
        add_build_system(component, BuildSystem::Cargo);
        add_toolchain(component, Toolchain::Rust);
        add_test_framework(component, TestFramework::CargoTest);
    } else if name == "package.json" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Node);
        add_build_system(component, BuildSystem::Npm);
        add_toolchain(component, Toolchain::Node);
        let text = read_manifest(root, generation, rel, refusals).unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        if lower.contains("\"jest\"") {
            add_test_framework(component, TestFramework::Jest);
        }
        if lower.contains("\"vitest\"") {
            add_test_framework(component, TestFramework::Vitest);
        }
        if lower.contains("\"mocha\"") {
            add_test_framework(component, TestFramework::Mocha);
        }
    } else if matches!(
        name,
        "pyproject.toml" | "setup.py" | "requirements.txt" | "Pipfile"
    ) {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Python);
        add_toolchain(component, Toolchain::Python);
        if name == "pyproject.toml" && probe_any(root, generation, rel, &["pytest"], refusals) {
            add_test_framework(component, TestFramework::Pytest);
        }
    } else if name == "go.mod" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Go);
        add_toolchain(component, Toolchain::Go);
        add_test_framework(component, TestFramework::GoTest);
    } else if name == "pom.xml" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Java);
        add_build_system(component, BuildSystem::Maven);
        add_toolchain(component, Toolchain::Jdk);
        add_test_framework(component, TestFramework::JUnit);
    } else if name == "build.gradle"
        || name == "build.gradle.kts"
        || name == "settings.gradle"
        || name == "settings.gradle.kts"
    {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::Java);
        add_build_system(component, BuildSystem::Gradle);
        add_toolchain(component, Toolchain::Jdk);
        add_test_framework(component, TestFramework::JUnit);
        if files.binary_search(&gradlew).is_ok() {
            add_toolchain(component, Toolchain::Gradle);
        }
    } else if name == "CMakeLists.txt" {
        let idf_component = probe_any(root, generation, rel, &["idf_component_register"], refusals);
        let target = if idf_component {
            nearest_key(components, &dir)
                .filter(|key| key != &dir)
                .unwrap_or_else(|| dir.clone())
        } else {
            dir.clone()
        };
        let component = component_at(components, &target);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::CMake);
        add_toolchain(component, Toolchain::CMake);
        if probe_any(
            root,
            generation,
            rel,
            &["find_package(zephyr", "zephyr/cmake"],
            refusals,
        ) {
            add_toolchain(component, Toolchain::ZephyrSdk);
            add_target(component, TargetProfile::Zephyr);
        }
        if idf_component
            || probe_any(
                root,
                generation,
                rel,
                &["idf_component_register", "esp-idf", "idf_path"],
                refusals,
            )
        {
            add_build_system(component, BuildSystem::EspIdf);
            add_toolchain(component, Toolchain::EspXtensa);
            add_target(component, TargetProfile::Esp32);
        }
        if probe_any(
            root,
            generation,
            rel,
            &["include(ctest", "enable_testing", "add_test("],
            refusals,
        ) {
            add_test_framework(component, TestFramework::CTest);
        }
        if probe_any(root, generation, rel, &["gtest", "googletest"], refusals) {
            add_test_framework(component, TestFramework::GoogleTest);
        }
        if probe_any(root, generation, rel, &["arm-none-eabi"], refusals) {
            add_toolchain(component, Toolchain::ArmNoneEabi);
            add_target(component, TargetProfile::ArmNoneEabi);
        }
    } else if matches!(name, "Makefile" | "makefile" | "GNUmakefile") {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::Make);
        add_toolchain(component, Toolchain::Make);
        let (test, check) = probe_make_targets(root, generation, &dir, files, refusals);
        if test {
            add_test_framework(component, TestFramework::MakeTest);
        }
        if check {
            add_test_framework(component, TestFramework::MakeCheck);
        }
    } else if name == "meson.build" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::Meson);
        add_toolchain(component, Toolchain::Meson);
    } else if name == "build.ninja" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::Ninja);
        add_toolchain(component, Toolchain::Ninja);
    } else if matches!(name, "MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel") {
        let component = component_at(components, &dir);
        add_build_system(component, BuildSystem::Bazel);
        add_toolchain(component, Toolchain::Bazel);
    } else if name == "platformio.ini" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::PlatformIO);
        add_toolchain(component, Toolchain::PlatformIO);
        let text = read_manifest(root, generation, rel, refusals).unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        if lower.contains("espressif32") || lower.contains("esp32") {
            add_toolchain(component, Toolchain::EspXtensa);
            add_target(component, TargetProfile::Esp32);
        }
        if lower.contains("ststm32")
            || lower.contains("atmelsam")
            || lower.contains("nordic")
            || lower.contains("teensy")
        {
            add_toolchain(component, Toolchain::ArmNoneEabi);
            add_target(component, TargetProfile::ArmNoneEabi);
        }
        if lower.contains("framework = unity") {
            add_test_framework(component, TestFramework::UnityCMock);
        }
        if lower.contains("test_dir") {
            add_test_framework(component, TestFramework::PlatformIoTest);
        }
    } else if name == "west.yml" || name == "west.yaml" {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::C);
        add_language(component, LanguageFamily::Cpp);
        add_build_system(component, BuildSystem::ZephyrWest);
        add_toolchain(component, Toolchain::West);
        add_toolchain(component, Toolchain::ZephyrSdk);
        add_target(component, TargetProfile::Zephyr);
        add_test_framework(component, TestFramework::ZephyrTwister);
    } else if name.ends_with(".sln") || name.ends_with(".csproj") {
        let component = component_at(components, &dir);
        add_language(component, LanguageFamily::CSharp);
        add_build_system(component, BuildSystem::DotNet);
        add_build_system(component, BuildSystem::MSBuild);
        add_toolchain(component, Toolchain::DotNet);
        add_toolchain(component, Toolchain::MSBuild);
        let lower = name.to_ascii_lowercase();
        if lower.contains("test") {
            add_test_framework(component, TestFramework::DotNetTest);
        }
        if name.ends_with(".csproj") {
            let text = read_manifest(root, generation, rel, refusals).unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if lower.contains("xunit") {
                add_test_framework(component, TestFramework::XUnit);
            }
            if lower.contains("nunit") {
                add_test_framework(component, TestFramework::NUnit);
            }
            if lower.contains("microsoft.net.test.sdk") {
                add_test_framework(component, TestFramework::DotNetTest);
            }
        }
    }
}

fn detect_evidence(
    root: &Path,
    generation: &CandidateGeneration,
    components: &mut BTreeMap<String, ProjectComponent>,
    files: &[String],
    refusals: &mut Vec<ManifestProbeRefusal>,
) {
    for rel in files {
        let dir = dir_of(rel).to_string();
        let name = basename(rel);
        if name.starts_with("sdkconfig") {
            let key = nearest_key(components, &dir).unwrap_or_else(|| dir.clone());
            let component = component_at(components, &key);
            add_build_system(component, BuildSystem::EspIdf);
            add_toolchain(component, Toolchain::EspXtensa);
            add_target(component, TargetProfile::Esp32);
        } else if name == "CMakePresets.json" {
            let key = nearest_key(components, &dir).unwrap_or_else(|| dir.clone());
            let component = component_at(components, &key);
            add_build_system(component, BuildSystem::CMake);
            add_toolchain(component, Toolchain::CMake);
            let text = read_manifest(root, generation, rel, refusals).unwrap_or_default();
            if text.to_ascii_lowercase().contains("arm-none-eabi") {
                add_toolchain(component, Toolchain::ArmNoneEabi);
                add_target(component, TargetProfile::ArmNoneEabi);
            }
        } else if name == "compile_commands.json" {
            let Some(key) = nearest_key(components, &dir) else {
                continue;
            };
            let component = component_at(components, &key);
            let text = read_manifest(root, generation, rel, refusals)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if text.contains("arm-none-eabi") {
                add_toolchain(component, Toolchain::ArmNoneEabi);
                add_target(component, TargetProfile::ArmNoneEabi);
            }
            if text.contains("xtensa") || text.contains("esp32") || text.contains("riscv32-esp") {
                add_toolchain(component, Toolchain::EspXtensa);
                add_target(component, TargetProfile::Esp32);
            }
        } else if name.ends_with(".cmake")
            && (name.to_ascii_lowercase().contains("toolchain") || basename(&dir) == "cmake")
        {
            let Some(key) = nearest_key(components, &dir) else {
                continue;
            };
            let component = component_at(components, &key);
            let text = read_manifest(root, generation, rel, refusals)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if text.contains("arm-none-eabi") {
                add_toolchain(component, Toolchain::ArmNoneEabi);
                add_target(component, TargetProfile::ArmNoneEabi);
            }
            if text.contains("xtensa") || text.contains("esp32") {
                add_toolchain(component, Toolchain::EspXtensa);
                add_target(component, TargetProfile::Esp32);
            }
            if text.contains("zephyr") {
                add_toolchain(component, Toolchain::ZephyrSdk);
                add_target(component, TargetProfile::Zephyr);
            }
        } else if matches!(name, "pytest.ini" | "conftest.py" | "tox.ini") {
            if let Some(key) = nearest_key(components, &dir) {
                add_test_framework(component_at(components, &key), TestFramework::Pytest);
            }
        } else if name.starts_with("jest.config.") {
            if let Some(key) = nearest_key(components, &dir) {
                add_test_framework(component_at(components, &key), TestFramework::Jest);
            }
        } else if name.starts_with("vitest.config.") {
            if let Some(key) = nearest_key(components, &dir) {
                add_test_framework(component_at(components, &key), TestFramework::Vitest);
            }
        }
    }
}

fn detect_languages(components: &mut BTreeMap<String, ProjectComponent>, files: &[String]) {
    for rel in files {
        let language = if rel.ends_with(".rs") {
            LanguageFamily::Rust
        } else if rel.ends_with(".ts") || rel.ends_with(".tsx") || rel.ends_with(".js") {
            LanguageFamily::Node
        } else if rel.ends_with(".py") {
            LanguageFamily::Python
        } else if rel.ends_with(".go") {
            LanguageFamily::Go
        } else if rel.ends_with(".java") {
            LanguageFamily::Java
        } else if rel.ends_with(".kt") || rel.ends_with(".kts") {
            LanguageFamily::Kotlin
        } else if rel.ends_with(".cs") {
            LanguageFamily::CSharp
        } else if rel.ends_with(".s") || rel.ends_with(".S") {
            LanguageFamily::Assembly
        } else if rel.ends_with(".c") {
            LanguageFamily::C
        } else if rel.ends_with(".cc")
            || rel.ends_with(".cpp")
            || rel.ends_with(".cxx")
            || rel.ends_with(".h")
            || rel.ends_with(".hh")
            || rel.ends_with(".hpp")
            || rel.ends_with(".hxx")
        {
            LanguageFamily::Cpp
        } else {
            continue;
        };
        let Some(key) = nearest_key(components, dir_of(rel)) else {
            continue;
        };
        add_language(component_at(components, &key), language);
    }
}

fn detect_ctest(components: &mut BTreeMap<String, ProjectComponent>, files: &[String]) {
    let mut additions: Vec<String> = Vec::new();
    for rel in files {
        let dir = dir_of(rel);
        // Ancestor walk (deepest first): the old nested scan selected the
        // DEEPEST component root that is an ancestor of the file's dir and
        // satisfies (CMake build system, discoverable relative path). All
        // such roots are ancestor prefixes, totally ordered by depth, so
        // the first satisfying hit is exactly that component — O(depth ·
        // log m) probes instead of O(m) per file.
        let mut candidate = dir;
        loop {
            count_probe();
            let is_cmake = components
                .get(candidate)
                .is_some_and(|component| component.build_systems.contains(&BuildSystem::CMake));
            if is_cmake {
                let discoverable = relative_to(candidate, rel).is_some_and(|relative| {
                    relative.starts_with("tests/")
                        || relative.starts_with("test/")
                        || relative.ends_with("CTestTestfile.cmake")
                });
                if discoverable {
                    additions.push(candidate.to_string());
                    break;
                }
            }
            match candidate.rsplit_once('/') {
                Some((parent, _)) => candidate = parent,
                None if !candidate.is_empty() => candidate = "",
                None => break,
            }
        }
    }
    additions.sort();
    additions.dedup();
    for key in additions {
        add_test_framework(component_at(components, &key), TestFramework::CTest);
    }
}

fn seal(mut component: ProjectComponent) -> ProjectComponent {
    component.languages.sort();
    component.languages.dedup();
    component.build_systems.sort();
    component.build_systems.dedup();
    component.toolchains.sort();
    component.toolchains.dedup();
    component.targets.sort();
    component.targets.dedup();
    component.test_frameworks.sort();
    component.test_frameworks.dedup();
    component
}

/// Detect EVERY project component of a repository from its bounded file map
/// plus bounded content probes under `root`. Components are ordered
/// most-specific root first, ties broken by path; every fact vector is
/// sorted and deduped, so detection is deterministic. Content probes are
/// admitted under a fresh [`CandidateGeneration`] over `root`; every probe
/// that cannot certify its content (unreadable/oversized/unstable) is
/// surfaced on [`ProjectProfile::manifest_refusals`] — never silently
/// treated as an absent manifest.
pub fn detect_project_profile(root: &Path, files: &[String]) -> ProjectProfile {
    let generation = CandidateGeneration::admit(root);
    detect_project_profile_at_generation(root, files, &generation)
}

/// [`detect_project_profile`] under an EXPLICITLY admitted candidate
/// generation (the caller ties the detection to the generation its root was
/// admitted under). A generation that no longer matches `root` refuses
/// every content probe typed (`unstable`), so content can never be certified
/// across generations.
pub fn detect_project_profile_at_generation(
    root: &Path,
    files: &[String],
    generation: &CandidateGeneration,
) -> ProjectProfile {
    let mut files_norm: Vec<String> = files.iter().filter_map(|f| normalize_rel(f)).collect();
    files_norm.sort();
    files_norm.dedup();
    let mut components: BTreeMap<String, ProjectComponent> = BTreeMap::new();
    let mut refusals: Vec<ManifestProbeRefusal> = Vec::new();
    for rel in &files_norm {
        detect_marker(
            root,
            generation,
            &mut components,
            &files_norm,
            rel,
            &mut refusals,
        );
    }
    detect_evidence(
        root,
        generation,
        &mut components,
        &files_norm,
        &mut refusals,
    );
    detect_languages(&mut components, &files_norm);
    detect_ctest(&mut components, &files_norm);
    let depth = |path: &Path| path.components().count();
    let mut profile: Vec<ProjectComponent> = components.into_values().map(seal).collect();
    profile.sort_by(|a, b| {
        depth(&b.root)
            .cmp(&depth(&a.root))
            .then_with(|| a.root.cmp(&b.root))
    });
    refusals.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(b.kind)));
    refusals.dedup();
    ProjectProfile {
        components: profile,
        manifest_refusals: refusals,
    }
}

impl From<crate::ProjectType> for ProjectProfile {
    fn from(project_type: crate::ProjectType) -> Self {
        use crate::ProjectType;
        let (languages, build_systems, toolchains, targets, test_frameworks) = match project_type {
            ProjectType::Rust => (
                vec![LanguageFamily::Rust],
                vec![BuildSystem::Cargo],
                vec![Toolchain::Rust],
                vec![],
                vec![TestFramework::CargoTest],
            ),
            ProjectType::Node => (
                vec![LanguageFamily::Node],
                vec![BuildSystem::Npm],
                vec![Toolchain::Node],
                vec![],
                vec![],
            ),
            ProjectType::Python => (
                vec![LanguageFamily::Python],
                vec![],
                vec![Toolchain::Python],
                vec![],
                vec![],
            ),
            ProjectType::Go => (
                vec![LanguageFamily::Go],
                vec![],
                vec![Toolchain::Go],
                vec![],
                vec![TestFramework::GoTest],
            ),
            ProjectType::Java => (
                vec![LanguageFamily::Java],
                vec![BuildSystem::Maven],
                vec![Toolchain::Jdk],
                vec![],
                vec![TestFramework::JUnit],
            ),
            ProjectType::Gradle => (
                vec![LanguageFamily::Java],
                vec![BuildSystem::Gradle],
                vec![Toolchain::Jdk],
                vec![],
                vec![TestFramework::JUnit],
            ),
            ProjectType::CMake => (
                vec![LanguageFamily::C, LanguageFamily::Cpp],
                vec![BuildSystem::CMake],
                vec![Toolchain::CMake],
                vec![],
                vec![],
            ),
            ProjectType::Make => (
                vec![LanguageFamily::C, LanguageFamily::Cpp],
                vec![BuildSystem::Make],
                vec![Toolchain::Make],
                vec![],
                vec![],
            ),
            ProjectType::Meson => (
                vec![LanguageFamily::C, LanguageFamily::Cpp],
                vec![BuildSystem::Meson],
                vec![Toolchain::Meson],
                vec![],
                vec![],
            ),
            ProjectType::Ninja => (
                vec![LanguageFamily::C, LanguageFamily::Cpp],
                vec![BuildSystem::Ninja],
                vec![Toolchain::Ninja],
                vec![],
                vec![],
            ),
            ProjectType::Bazel => (
                vec![],
                vec![BuildSystem::Bazel],
                vec![Toolchain::Bazel],
                vec![],
                vec![],
            ),
            ProjectType::DotNet => (
                vec![LanguageFamily::CSharp],
                vec![BuildSystem::DotNet, BuildSystem::MSBuild],
                vec![Toolchain::DotNet, Toolchain::MSBuild],
                vec![],
                vec![],
            ),
            ProjectType::Unknown => return Self::default(),
        };
        Self {
            components: vec![ProjectComponent {
                root: PathBuf::new(),
                languages,
                build_systems,
                toolchains,
                targets,
                test_frameworks,
            }],
            manifest_refusals: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_check(
    out: &mut Vec<CheckSpec>,
    component: &ProjectComponent,
    base_id: &str,
    kind: CheckKind,
    category: CheckCategory,
    program: &str,
    args: &[&str],
    affects: Vec<String>,
    required: bool,
) {
    out.push(CheckSpec {
        id: component_id(component, base_id),
        kind,
        category,
        program: OsString::from(program),
        args: args.iter().map(|arg| OsString::from(*arg)).collect(),
        cwd_rel: component.root.clone(),
        affects,
        required,
    });
}

fn c_sources(files: &[String]) -> bool {
    files.iter().any(|file| {
        [
            ".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx", ".s", ".S", ".asm", ".rs",
        ]
        .iter()
        .any(|extension| file.ends_with(extension))
    })
}

fn named(files: &[String], names: &[&str]) -> bool {
    files.iter().any(|file| names.contains(&basename(file)))
}

fn test_dir_files(files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|file| {
            under_dir(file, "test")
                || under_dir(file, "tests")
                || under_dir(file, "spec")
                || [
                    ".test.ts",
                    ".test.tsx",
                    ".spec.ts",
                    ".spec.tsx",
                    ".test.js",
                    ".test.jsx",
                    ".spec.js",
                    ".spec.jsx",
                    ".test.mjs",
                    ".test.cjs",
                    ".spec.mjs",
                    ".spec.cjs",
                ]
                .iter()
                .any(|suffix| file.ends_with(suffix))
        })
        .cloned()
        .collect()
}

/// TRUE when the component's package.json declares a non-empty `test` script
/// under `scripts`. Bounded read; a missing/unreadable/oversized manifest is
/// `false` (no derived Node check rather than a guessed command). This is
/// what lets a change in `src/` still derive the component's real test
/// command: the old derivation only looked at the changed files, so a
/// JS/TS source edit produced no checks at all and the task could never
/// reach VerifiedComplete.
fn node_has_test_script(component: &ProjectComponent, workspace_root: Option<&Path>) -> bool {
    let Some(root) = workspace_root else {
        return false;
    };
    let rel = component.root.join("package.json");
    let rel = rel.to_string_lossy();
    let Ok(bytes) = crate::manifest::read_manifest_bounded(root, &rel, 256 * 1024) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    json.get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|test| test.as_str())
        .is_some_and(|test| !test.trim().is_empty())
}

fn derive_cargo(
    out: &mut Vec<CheckSpec>,
    component: &ProjectComponent,
    local: &[String],
    workspace_root: Option<&Path>,
) {
    let rust: Vec<String> = local
        .iter()
        .filter(|file| file.ends_with(".rs"))
        .cloned()
        .collect();
    if rust.is_empty() {
        return;
    }
    push_check(
        out,
        component,
        "rust_check",
        CheckKind::Compile,
        CheckCategory::Quick,
        "cargo",
        &["check"],
        rust.clone(),
        true,
    );
    let test_related: Vec<String> = rust
        .iter()
        .filter(|file| is_test_related(file))
        .cloned()
        .collect();
    if let Some(first) = test_related.first() {
        if let Some(stem) = rust_test_filter(first) {
            push_check(
                out,
                component,
                &format!("rust_test:{stem}"),
                CheckKind::Test,
                CheckCategory::Unit,
                "cargo",
                &["test", &stem],
                test_related,
                true,
            );
        }
    } else if let Some(workspace_root) = workspace_root {
        // INLINE tests in a non-test-named source file (the common
        // `#[cfg(test)] mod tests` layout): the contract compiler discovers
        // these selectors and binds required test-execution obligations to
        // them. Without a derived `cargo test <selector>` the obligation can
        // never be discharged and a correct change blocks forever. Read only
        // the changed Rust files, bounded by count and bytes; the shared
        // parser is the one the contract compiler uses, so demanded and
        // executable selectors cannot drift.
        const MAX_INLINE_TEST_FILES: usize = 64;
        const MAX_INLINE_TEST_FILE_BYTES: u64 = 1024 * 1024;
        const MAX_INLINE_TEST_CHECKS: usize = 16;
        let mut selectors: Vec<String> = Vec::new();
        let component_root = workspace_root.join(&component.root);
        for file in rust.iter().take(MAX_INLINE_TEST_FILES) {
            // Changed paths may arrive absolute (the shadow root of an
            // isolated drive) or workspace-relative. Normalize to the
            // component-relative spelling the selector parser understands.
            let relative = std::path::Path::new(file)
                .strip_prefix(&component_root)
                .unwrap_or_else(|_| std::path::Path::new(file));
            let path = component_root.join(relative);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > MAX_INLINE_TEST_FILE_BYTES {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let relative = relative.to_string_lossy();
            for selector in
                faktor_core::contract_compile::rust_test_selectors_for_path(&relative, &content)
            {
                if !selectors.contains(&selector) {
                    selectors.push(selector);
                }
            }
        }
        selectors.sort();
        selectors.dedup();
        let discovered = !selectors.is_empty();
        for selector in selectors.into_iter().take(MAX_INLINE_TEST_CHECKS) {
            push_check(
                out,
                component,
                &format!("rust_test:{selector}"),
                CheckKind::Test,
                CheckCategory::Unit,
                "cargo",
                &["test", selector.as_str()],
                rust.clone(),
                true,
            );
        }
        if !discovered {
            // No inline selectors in the CHANGED file, but the component's
            // integration tests (`tests/*.rs`) and any model-added tests are
            // still part of the change's contract. `--lib` skipped them, so
            // a verified candidate could carry a failing integration test
            // (observed: a config migration passed `--lib` while
            // tests/config.rs failed five assertions). `--tests` runs every
            // test target; the check is REQUIRED.
            push_check(
                out,
                component,
                "rust_test_workspace",
                CheckKind::Test,
                CheckCategory::Unit,
                "cargo",
                &["test", "--tests"],
                Vec::new(),
                true,
            );
        }
    } else {
        // Rootless callers (native `run_tests` operations over an unknown
        // checkout) keep the historical optional library-test derivation.
        push_check(
            out,
            component,
            "rust_test_lib",
            CheckKind::Test,
            CheckCategory::Unit,
            "cargo",
            &["test", "--lib"],
            Vec::new(),
            false,
        );
    }
}

fn derive_node(
    out: &mut Vec<CheckSpec>,
    component: &ProjectComponent,
    local: &[String],
    workspace_root: Option<&Path>,
) {
    let sources: Vec<String> = local
        .iter()
        .filter(|file| file.ends_with(".ts") || file.ends_with(".tsx"))
        .cloned()
        .collect();
    if !sources.is_empty() {
        push_check(
            out,
            component,
            "node_tsc",
            CheckKind::Compile,
            CheckCategory::Quick,
            "npx",
            &["tsc", "--noEmit"],
            sources,
            true,
        );
    }
    let tests = test_dir_files(local);
    // A source-only change is still governed by the component's declared
    // test script: derive `npm test` even when no changed path is itself a
    // test file. Without this, a JS `src/` edit produced an EMPTY check set,
    // stayed unverified by construction, and the verified candidate was
    // never landed.
    let node_sources: Vec<String> = local
        .iter()
        .filter(|file| {
            [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx"]
                .iter()
                .any(|suffix| file.ends_with(suffix))
        })
        .cloned()
        .collect();
    let declared = !node_sources.is_empty() && node_has_test_script(component, workspace_root);
    if !tests.is_empty() || declared {
        let affects = if tests.is_empty() {
            node_sources
        } else {
            tests
        };
        push_check(
            out,
            component,
            "node_test",
            CheckKind::Test,
            CheckCategory::Unit,
            "npm",
            &["test"],
            affects,
            true,
        );
    }
}

fn derive_python(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let python: Vec<String> = local
        .iter()
        .filter(|file| file.ends_with(".py"))
        .cloned()
        .collect();
    let mut tokens: Vec<String> = python
        .iter()
        .filter_map(|file| compileall_token(file))
        .collect();
    tokens.sort();
    tokens.dedup();
    for token in tokens {
        let affected: Vec<String> = python
            .iter()
            .filter(|file| compileall_token(file).as_deref() == Some(token.as_str()))
            .cloned()
            .collect();
        push_check(
            out,
            component,
            &format!("python_compile:{token}"),
            CheckKind::Compile,
            CheckCategory::Quick,
            "python",
            &["-m", "compileall", "-q", &token],
            affected,
            true,
        );
    }
    let tests: Vec<String> = python
        .iter()
        .filter(|file| under_dir(file, "tests"))
        .cloned()
        .collect();
    if !tests.is_empty() {
        push_check(
            out,
            component,
            "python_test",
            CheckKind::Test,
            CheckCategory::Unit,
            "pytest",
            &["-q"],
            tests,
            false,
        );
    }
}

fn derive_go(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let go: Vec<String> = local
        .iter()
        .filter(|file| file.ends_with(".go"))
        .cloned()
        .collect();
    if go.is_empty() {
        return;
    }
    push_check(
        out,
        component,
        "go_build",
        CheckKind::Compile,
        CheckCategory::Quick,
        "go",
        &["build", "./..."],
        go.clone(),
        true,
    );
    let test_files = go.iter().any(|file| file.ends_with("_test.go"));
    push_check(
        out,
        component,
        "go_test",
        CheckKind::Test,
        CheckCategory::Unit,
        "go",
        &["test", "./..."],
        go,
        test_files,
    );
}

fn derive_maven(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let java: Vec<String> = local
        .iter()
        .filter(|file| file.ends_with(".java"))
        .cloned()
        .collect();
    if java.is_empty() {
        return;
    }
    push_check(
        out,
        component,
        "java_compile",
        CheckKind::Compile,
        CheckCategory::Quick,
        "mvn",
        &["-q", "-DskipTests", "compile"],
        java.clone(),
        true,
    );
    let tests: Vec<String> = java
        .iter()
        .filter(|file| is_test_related(file))
        .cloned()
        .collect();
    push_check(
        out,
        component,
        "java_test",
        CheckKind::Test,
        CheckCategory::Unit,
        "mvn",
        &["-q", "test"],
        if tests.is_empty() { java } else { tests },
        false,
    );
}

fn derive_gradle(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = local.iter().any(|file| {
        file.ends_with(".java")
            || file.ends_with(".kt")
            || file.ends_with(".kts")
            || named(
                local,
                &["build.gradle", "build.gradle.kts", "settings.gradle.kts"],
            )
    });
    if !touched {
        return;
    }
    let program = if component.toolchains.contains(&Toolchain::Gradle) {
        "./gradlew"
    } else {
        "gradle"
    };
    push_check(
        out,
        component,
        "gradle_classes",
        CheckKind::Compile,
        CheckCategory::Unit,
        program,
        &["classes"],
        Vec::new(),
        true,
    );
    let tests = local.iter().any(|file| {
        file.contains("/src/test/") || file.ends_with("Test.kt") || file.ends_with("Test.java")
    });
    if tests {
        push_check(
            out,
            component,
            "gradle_test",
            CheckKind::Test,
            CheckCategory::Full,
            program,
            &["test"],
            Vec::new(),
            true,
        );
    }
}

fn derive_cmake(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local)
        || named(local, &["CMakeLists.txt", "CMakePresets.json"])
        || local.iter().any(|file| file.ends_with(".cmake"));
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "cmake_configure",
        CheckKind::Compile,
        CheckCategory::Unit,
        "cmake",
        &["-S", ".", "-B", BUILD_DIR],
        Vec::new(),
        true,
    );
    push_check(
        out,
        component,
        "cmake_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "cmake",
        &["--build", BUILD_DIR],
        Vec::new(),
        true,
    );
    if component.test_frameworks.contains(&TestFramework::CTest) {
        push_check(
            out,
            component,
            "cmake_ctest",
            CheckKind::Test,
            CheckCategory::Full,
            "ctest",
            &["--test-dir", BUILD_DIR, "--output-on-failure"],
            Vec::new(),
            true,
        );
    }
}

fn derive_make(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local)
        || named(local, &["Makefile", "makefile", "GNUmakefile"])
        || local.iter().any(|file| file.ends_with(".cmake"));
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "make_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "make",
        &["-j"],
        Vec::new(),
        true,
    );
    if component.test_frameworks.contains(&TestFramework::MakeTest) {
        push_check(
            out,
            component,
            "make_test",
            CheckKind::Test,
            CheckCategory::Full,
            "make",
            &["test"],
            Vec::new(),
            true,
        );
    }
    if component
        .test_frameworks
        .contains(&TestFramework::MakeCheck)
    {
        push_check(
            out,
            component,
            "make_check",
            CheckKind::Test,
            CheckCategory::Full,
            "make",
            &["check"],
            Vec::new(),
            true,
        );
    }
}

fn derive_meson(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local) || named(local, &["meson.build"]);
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "meson_setup",
        CheckKind::Compile,
        CheckCategory::Unit,
        "meson",
        &["setup", BUILD_DIR],
        Vec::new(),
        true,
    );
    push_check(
        out,
        component,
        "meson_compile",
        CheckKind::Compile,
        CheckCategory::Unit,
        "meson",
        &["compile", "-C", BUILD_DIR],
        Vec::new(),
        true,
    );
    push_check(
        out,
        component,
        "meson_test",
        CheckKind::Test,
        CheckCategory::Full,
        "meson",
        &["test", "-C", BUILD_DIR],
        Vec::new(),
        true,
    );
}

fn derive_ninja(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local) || named(local, &["build.ninja"]);
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "ninja_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "ninja",
        &["-C", "."],
        Vec::new(),
        true,
    );
    push_check(
        out,
        component,
        "ninja_test",
        CheckKind::Test,
        CheckCategory::Full,
        "ninja",
        &["-C", ".", "test"],
        Vec::new(),
        false,
    );
}

fn derive_bazel(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local)
        || local.iter().any(|file| file.ends_with(".bzl"))
        || named(
            local,
            &["MODULE.bazel", "WORKSPACE", "BUILD", "BUILD.bazel"],
        );
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "bazel_test_affected",
        CheckKind::Test,
        CheckCategory::Full,
        "bazel",
        &["test", "//..."],
        Vec::new(),
        true,
    );
}

fn derive_dotnet(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = local
        .iter()
        .any(|file| file.ends_with(".cs") || file.ends_with(".csproj") || file.ends_with(".sln"));
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "dotnet_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "dotnet",
        &["build"],
        Vec::new(),
        true,
    );
    let tests = component.test_frameworks.iter().any(|framework| {
        matches!(
            framework,
            TestFramework::DotNetTest | TestFramework::XUnit | TestFramework::NUnit
        )
    });
    if tests {
        push_check(
            out,
            component,
            "dotnet_test",
            CheckKind::Test,
            CheckCategory::Full,
            "dotnet",
            &["test"],
            Vec::new(),
            true,
        );
    }
}

fn derive_platformio(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local) || named(local, &["platformio.ini"]);
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "pio_run",
        CheckKind::Compile,
        CheckCategory::Unit,
        "pio",
        &["run"],
        Vec::new(),
        true,
    );
    let tests = test_dir_files(local);
    if !tests.is_empty() {
        push_check(
            out,
            component,
            "pio_test",
            CheckKind::Test,
            CheckCategory::Full,
            "pio",
            &["test"],
            tests,
            true,
        );
    }
}

fn derive_zephyr(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local) || named(local, &["west.yml", "west.yaml"]);
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "west_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "west",
        &["build"],
        Vec::new(),
        true,
    );
    let tests = test_dir_files(local);
    if !tests.is_empty() {
        push_check(
            out,
            component,
            "west_twister",
            CheckKind::Test,
            CheckCategory::Full,
            "west",
            &["twister", "-T", "tests"],
            tests,
            true,
        );
    }
}

fn derive_espidf(out: &mut Vec<CheckSpec>, component: &ProjectComponent, local: &[String]) {
    let touched = c_sources(local)
        || local.iter().any(|file| {
            basename(file).starts_with("sdkconfig") || basename(file) == "CMakeLists.txt"
        });
    if !touched {
        return;
    }
    push_check(
        out,
        component,
        "idf_py_build",
        CheckKind::Compile,
        CheckCategory::Unit,
        "idf.py",
        &["build"],
        Vec::new(),
        true,
    );
}

fn derive_component_checks(
    component: &ProjectComponent,
    local: &[String],
    workspace_root: Option<&Path>,
) -> Vec<CheckSpec> {
    let mut out: Vec<CheckSpec> = Vec::new();
    let has = |build_system: BuildSystem| component.build_systems.contains(&build_system);
    let lang = |language: LanguageFamily| component.languages.contains(&language);
    if has(BuildSystem::Cargo) {
        derive_cargo(&mut out, component, local, workspace_root);
    }
    if has(BuildSystem::Npm) {
        derive_node(&mut out, component, local, workspace_root);
    }
    if has(BuildSystem::Maven) {
        derive_maven(&mut out, component, local);
    }
    if has(BuildSystem::Gradle) {
        derive_gradle(&mut out, component, local);
    }
    let embedded_native = has(BuildSystem::ZephyrWest)
        || has(BuildSystem::EspIdf)
        || component.targets.contains(&TargetProfile::Zephyr);
    if has(BuildSystem::CMake) && !embedded_native {
        derive_cmake(&mut out, component, local);
    } else if has(BuildSystem::Make) {
        derive_make(&mut out, component, local);
    } else if has(BuildSystem::Meson) {
        derive_meson(&mut out, component, local);
    } else if has(BuildSystem::Ninja) {
        derive_ninja(&mut out, component, local);
    } else if has(BuildSystem::Bazel) {
        derive_bazel(&mut out, component, local);
    }
    if has(BuildSystem::PlatformIO) {
        derive_platformio(&mut out, component, local);
    }
    if has(BuildSystem::ZephyrWest) || component.targets.contains(&TargetProfile::Zephyr) {
        derive_zephyr(&mut out, component, local);
    }
    if has(BuildSystem::EspIdf) {
        derive_espidf(&mut out, component, local);
    }
    if has(BuildSystem::DotNet) || has(BuildSystem::MSBuild) {
        derive_dotnet(&mut out, component, local);
    }
    if component.build_systems.is_empty() {
        if lang(LanguageFamily::Python) {
            derive_python(&mut out, component, local);
        }
        if lang(LanguageFamily::Go) {
            derive_go(&mut out, component, local);
        }
    }
    out
}

/// Derive the checks a change requires from a multi-component profile.
/// Every changed file maps to its owning component (longest root prefix) and
/// every owning component contributes ALL of its applicable check families.
/// Identical specs are deduped; conflicting ids and cap overruns are typed
/// errors — required semantic coverage is never truncated.
pub fn derive_checks(
    profile: &ProjectProfile,
    changed: &[PathBuf],
) -> Result<Vec<CheckSpec>, VerificationDerivationError> {
    derive_checks_with_limits(profile, changed, DerivationLimits::default())
}

/// Derive checks against an EXPLICIT workspace root. The root is required to
/// discover inline `#[cfg(test)]` selectors in changed source files; the
/// rootless legacy entry point derives no inline test checks (tests using
/// relative profiles keep their historical output).
pub fn derive_checks_at_root(
    profile: &ProjectProfile,
    changed: &[PathBuf],
    root: &Path,
) -> Result<Vec<CheckSpec>, VerificationDerivationError> {
    derive_checks_with_limits_and_root(profile, changed, DerivationLimits::default(), Some(root))
}

/// [`derive_checks`] under explicit caps (adversarial tests and callers with
/// tighter execution budgets).
pub fn derive_checks_with_limits(
    profile: &ProjectProfile,
    changed: &[PathBuf],
    limits: DerivationLimits,
) -> Result<Vec<CheckSpec>, VerificationDerivationError> {
    derive_checks_with_limits_and_root(profile, changed, limits, None)
}

fn derive_checks_with_limits_and_root(
    profile: &ProjectProfile,
    changed: &[PathBuf],
    limits: DerivationLimits,
    workspace_root: Option<&Path>,
) -> Result<Vec<CheckSpec>, VerificationDerivationError> {
    // A content-deciding manifest probe that could not certify its content
    // (unreadable/oversized/unstable) means the derived check set would be
    // silently incomplete: refuse typed instead of deriving a smaller suite.
    if let Some(refusal) = profile.manifest_refusals.first() {
        return Err(VerificationDerivationError::ManifestProbeRefused {
            path: refusal.path.clone(),
            detail: refusal.detail.clone(),
        });
    }
    let mut changed_norm: Vec<String> = changed
        .iter()
        .filter_map(|path| normalize_rel(&path.to_string_lossy()))
        .collect();
    changed_norm.sort();
    changed_norm.dedup();
    let mut per_component: Vec<Vec<String>> = vec![Vec::new(); profile.components.len()];
    for (file, owner) in changed_norm
        .iter()
        .zip(profile.owner_indices(changed_norm.iter().map(|f| Path::new(f.as_str()))))
    {
        if let Some(index) = owner {
            per_component[index].push(file.clone());
        }
    }
    let mut specs: Vec<CheckSpec> = Vec::new();
    for (component, local) in profile.components.iter().zip(per_component.iter()) {
        if local.is_empty() {
            continue;
        }
        specs.extend(derive_component_checks(component, local, workspace_root));
    }
    // Dedupe by id with ONE map lookup per spec (spec counts are attacker
    // influenced before the MAX_CHECKS cap check, so the old O(k^2)
    // first-match scan is not admissible): first-seen order is preserved
    // exactly (specs push in iteration order), an identical spec is skipped,
    // and the same id with different bytes is the same typed conflict.
    let mut deduped: Vec<CheckSpec> = Vec::new();
    let mut index_by_id: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for spec in specs {
        if let Some(&index) = index_by_id.get(spec.id.as_str()) {
            if deduped[index] == spec {
                continue;
            }
            return Err(VerificationDerivationError::ConflictingCheckId { id: spec.id });
        }
        index_by_id.insert(spec.id.clone(), deduped.len());
        deduped.push(spec);
    }
    // Required checks first; within a group the derivation order (component
    // order, then family order) is preserved — deterministic, and stable
    // across runs of the same change.
    deduped.sort_by_key(|spec| !spec.required);
    if deduped.len() > limits.max_checks {
        return Err(VerificationDerivationError::TooManyChecks {
            count: deduped.len(),
            max: limits.max_checks,
        });
    }
    let estimated: Duration = deduped
        .iter()
        .map(|spec| nominal_budget(spec.category))
        .sum();
    if estimated > limits.max_total_wall {
        return Err(VerificationDerivationError::TotalWallBudgetExceeded {
            estimated,
            max: limits.max_total_wall,
        });
    }
    let jobs = deduped
        .iter()
        .filter(|spec| spec.category == CheckCategory::Full)
        .count();
    if jobs > limits.max_simultaneous_jobs {
        return Err(VerificationDerivationError::TooManySimultaneousJobs {
            jobs,
            max: limits.max_simultaneous_jobs,
        });
    }
    Ok(deduped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{acceptance, Acceptance, Check, CheckKind as LegacyKind};

    fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut names = Vec::new();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            names.push(name.to_string());
        }
        (dir, names)
    }

    fn mixed_repo() -> (tempfile::TempDir, Vec<String>) {
        fixture(&[
            ("Cargo.toml", "[package]\nname = \"root\"\n"),
            ("src/lib.rs", "pub fn f() {}\n"),
            (
                "firmware/CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(fw C CXX)\n",
            ),
            (
                "firmware/platformio.ini",
                "[env:esp32dev]\nplatform = espressif32\nboard = esp32dev\n",
            ),
            ("firmware/src/main.cpp", "int main() { return 0; }\n"),
            (
                "desktop/app.csproj",
                "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>",
            ),
            ("desktop/Program.cs", "class P { static void Main() {} }\n"),
            ("web/package.json", "{\"scripts\":{\"test\":\"jest\"}}"),
            ("web/src/app.ts", "export const x = 1;\n"),
        ])
    }

    fn roots(profile: &ProjectProfile) -> Vec<String> {
        profile
            .components
            .iter()
            .map(|c| c.root.to_string_lossy().into_owned())
            .collect()
    }

    fn component_for<'a>(profile: &'a ProjectProfile, root: &str) -> &'a ProjectComponent {
        profile
            .components
            .iter()
            .find(|c| c.root.as_path() == Path::new(root))
            .unwrap_or_else(|| panic!("no component {root:?} in {:?}", roots(profile)))
    }

    fn ids(specs: &[CheckSpec]) -> Vec<String> {
        specs.iter().map(|spec| spec.id.clone()).collect()
    }

    fn required_ids(specs: &[CheckSpec]) -> Vec<String> {
        specs
            .iter()
            .filter(|spec| spec.required)
            .map(|spec| spec.id.clone())
            .collect()
    }

    // ------------------------------------------- independent audit probes
    //
    // The perf wave replaced per-file full component scans with ancestor
    // walks and a one-pass spec-id map. These probes re-implement the PRE-fix
    // algorithms verbatim in-test and require identical outputs on
    // adversarial roots/paths and component permutations.

    fn probe_component(
        root: &str,
        languages: Vec<LanguageFamily>,
        cmake: bool,
    ) -> ProjectComponent {
        ProjectComponent {
            root: PathBuf::from(root),
            languages,
            build_systems: if cmake {
                vec![BuildSystem::CMake]
            } else {
                vec![BuildSystem::Cargo]
            },
            toolchains: if cmake {
                vec![Toolchain::CMake]
            } else {
                vec![Toolchain::Rust]
            },
            targets: Vec::new(),
            test_frameworks: Vec::new(),
        }
    }

    /// PRE-FIX `owner_index`: the full per-file component scan, verbatim.
    fn oracle_owner_index(profile: &ProjectProfile, file: &str) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (index, component) in profile.components.iter().enumerate() {
            let root = component.root.to_string_lossy();
            if !is_under(&root, file) {
                continue;
            }
            let depth = root.matches('/').count() + usize::from(!root.is_empty());
            if best.is_none_or(|(best_depth, _)| depth > best_depth) {
                best = Some((depth, index));
            }
        }
        best.map(|(_, index)| index)
    }

    /// PRE-FIX `nearest_key`: the full per-dir key scan, verbatim.
    fn probe_old_nearest_key(
        components: &BTreeMap<String, ProjectComponent>,
        dir: &str,
    ) -> Option<String> {
        components
            .keys()
            .filter(|root| is_under(root, dir))
            .max_by_key(|root| root.matches('/').count() + usize::from(!root.is_empty()))
            .cloned()
    }

    #[test]
    fn owner_indices_match_the_full_scan_on_adversarial_paths() {
        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state >> 33
        }
        let roots = [
            "",
            "pkg",
            "pkg/sub",
            "a:b",
            "a_b",
            "颜色",
            "pkg/",
            "pkg//sub",
            "..",
            ".",
            "PKG",
            "pkg/sub/deep",
            "pkg/SUB",
        ];
        let files = [
            "pkg/x.c",
            "pkg",
            "pkg/",
            "pkg//x.c",
            "pkg/./x.c",
            "pkg/../x.c",
            "pkg/sub/y.c",
            "pkg/sub/deep/z.c",
            "pkg/SUB/y.c",
            "a:b/z.c",
            "a_b/w.c",
            "颜色/文件.c",
            "颜色/文件",
            "",
            "..",
            "./x",
            "pkg/Ünïcode.c",
            "PKG/X.C",
            "pkg/sub",
            "pkg/sub/",
        ];
        let mut state = 0xA11CE_u64;
        for round in 0..300 {
            let count = 1 + (lcg(&mut state) as usize) % 6;
            let components: Vec<ProjectComponent> = (0..count)
                .map(|_| {
                    let root = roots[(lcg(&mut state) as usize) % roots.len()];
                    probe_component(
                        root,
                        vec![LanguageFamily::Rust],
                        lcg(&mut state).is_multiple_of(2),
                    )
                })
                .collect();
            let profile = ProjectProfile {
                components,
                manifest_refusals: Vec::new(),
            };
            let paths: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(*f)).collect();
            let walked = profile.owner_indices(paths.iter().map(|p| p.as_path()));
            for (file, got) in files.iter().zip(walked) {
                assert_eq!(
                    got,
                    oracle_owner_index(&profile, file),
                    "round {round}: owner of {file:?} drifted (roots {:?})",
                    profile
                        .components
                        .iter()
                        .map(|c| c.root.display().to_string())
                        .collect::<Vec<_>>()
                );
            }
            // `nearest_key` over the same roots: the ancestor walk must equal
            // the old max-depth scan key-by-key.
            let mut map: BTreeMap<String, ProjectComponent> = BTreeMap::new();
            for component in &profile.components {
                let key = component.root.to_string_lossy().into_owned();
                map.entry(key).or_insert_with(|| component.clone());
            }
            for dir in [
                "", "pkg", "pkg/", "pkg/sub", "pkg/sub/", "a:b", "x/y", "..", ".",
            ] {
                assert_eq!(
                    nearest_key(&map, dir),
                    probe_old_nearest_key(&map, dir),
                    "round {round}: nearest_key({dir:?}) drifted"
                );
            }
        }
    }

    #[test]
    fn derived_checks_match_a_naive_ownership_dedupe_pipeline() {
        // Components with roots that collide after id sanitization
        // (`a:b` vs `a_b`), nested ownership, and duplicates.
        let components = [
            probe_component("pkg", vec![LanguageFamily::C], true),
            probe_component("pkg/sub", vec![LanguageFamily::C], true),
            probe_component("a:b", vec![LanguageFamily::C], true),
            probe_component("a_b", vec![LanguageFamily::C], true),
            probe_component("pkg", vec![LanguageFamily::C], true),
            probe_component("颜色", vec![LanguageFamily::Rust], false),
        ];
        let changed: Vec<PathBuf> = [
            "pkg/x.c",
            "pkg/sub/y.c",
            "a:b/z.c",
            "a_b/w.c",
            "颜色/lib.rs",
            "pkg/Ünïcode.c",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let mut state = 0xD00D_u64;
        for round in 0..120 {
            // Deterministic permutations of the component list.
            let mut order: Vec<usize> = (0..components.len()).collect();
            for i in (1..order.len()).rev() {
                let j = (state as usize) % (i + 1);
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                order.swap(i, j);
            }
            let permuted: Vec<ProjectComponent> =
                order.iter().map(|i| components[*i].clone()).collect();
            let profile = ProjectProfile {
                components: permuted.clone(),
                manifest_refusals: Vec::new(),
            };
            // Oracle: ownership by the PRE-FIX full scan, per-component
            // derivation through the public single-component API, then the
            // PRE-FIX pairwise dedupe and the same required-first sort.
            let normalized: Vec<String> = changed
                .iter()
                .filter_map(|p| normalize_rel(&p.to_string_lossy()))
                .collect();
            let mut per_component: Vec<Vec<String>> = vec![Vec::new(); profile.components.len()];
            for file in &normalized {
                if let Some(owner) = oracle_owner_index(&profile, file) {
                    per_component[owner].push(file.clone());
                }
            }
            let mut specs: Vec<CheckSpec> = Vec::new();
            for (component, local) in profile.components.iter().zip(per_component.iter()) {
                if local.is_empty() {
                    continue;
                }
                let single = ProjectProfile {
                    components: vec![component.clone()],
                    manifest_refusals: Vec::new(),
                };
                let local_paths: Vec<PathBuf> = local.iter().map(PathBuf::from).collect();
                specs.extend(derive_checks(&single, &local_paths).unwrap());
            }
            let mut deduped: Vec<CheckSpec> = Vec::new();
            let mut oracle_error: Option<String> = None;
            for spec in specs {
                match deduped.iter().find(|existing| existing.id == spec.id) {
                    Some(existing) if *existing == spec => continue,
                    Some(_) => {
                        oracle_error = Some(spec.id);
                        break;
                    }
                    None => deduped.push(spec),
                }
            }
            match oracle_error {
                Some(expected_id) => match derive_checks(&profile, &changed) {
                    Err(VerificationDerivationError::ConflictingCheckId { id }) => {
                        assert_eq!(id, expected_id, "round {round}: conflict id drifted");
                    }
                    other => {
                        panic!("round {round}: oracle says conflict {expected_id}, got {other:?}")
                    }
                },
                None => {
                    deduped.sort_by_key(|spec| !spec.required);
                    let derived = derive_checks(&profile, &changed).unwrap();
                    assert_eq!(
                        derived, deduped,
                        "round {round}: derived checks drifted under permutation {order:?}"
                    );
                }
            }
            // The direct owner_indices output must equal the naive scan on
            // this exact corpus, too.
            let paths: Vec<PathBuf> = changed.clone();
            let owners = profile.owner_indices(paths.iter().map(|p| p.as_path()));
            for (path, owner) in changed.iter().zip(owners) {
                let file = path.to_string_lossy();
                assert_eq!(
                    owner,
                    oracle_owner_index(&profile, &file),
                    "round {round}: ownership of {file} drifted"
                );
            }
        }
    }

    fn mirror(spec: &CheckSpec) -> Check {
        let kind = match spec.kind {
            CheckKind::Compile => LegacyKind::Compile,
            CheckKind::Test => LegacyKind::Test,
            CheckKind::Lint => LegacyKind::Lint,
        };
        let mut command = spec.program.to_string_lossy().into_owned();
        for arg in &spec.args {
            command.push(' ');
            command.push_str(&arg.to_string_lossy());
        }
        Check {
            id: spec.id.clone(),
            kind,
            command,
            affects: spec.affects.clone(),
            required: spec.required,
        }
    }

    #[test]
    fn mixed_repo_detects_every_component_most_specific_first() {
        let (dir, files) = mixed_repo();
        let profile = detect_project_profile(dir.path(), &files);
        assert_eq!(
            roots(&profile),
            vec!["desktop", "firmware", "web", ""],
            "most-specific roots first, deterministic tie-break by path"
        );
        let firmware = component_for(&profile, "firmware");
        assert!(firmware.build_systems.contains(&BuildSystem::CMake));
        assert!(firmware.build_systems.contains(&BuildSystem::PlatformIO));
        assert!(firmware.toolchains.contains(&Toolchain::EspXtensa));
        assert!(firmware.targets.contains(&TargetProfile::Esp32));
        assert!(component_for(&profile, "desktop")
            .build_systems
            .contains(&BuildSystem::DotNet));
        let web = component_for(&profile, "web");
        assert!(web.build_systems.contains(&BuildSystem::Npm));
        assert!(web.test_frameworks.contains(&TestFramework::Jest));
        let root = component_for(&profile, "");
        assert_eq!(root.build_systems, vec![BuildSystem::Cargo]);
        assert_eq!(root.languages, vec![LanguageFamily::Rust]);
    }

    #[test]
    fn detection_is_order_independent_and_reproducible() {
        let (dir, mut files) = mixed_repo();
        let first = detect_project_profile(dir.path(), &files);
        files.reverse();
        let reversed = detect_project_profile(dir.path(), &files);
        assert_eq!(first, reversed);
    }

    #[test]
    fn one_changed_tree_contributes_its_own_required_family() {
        let (dir, files) = mixed_repo();
        let profile = detect_project_profile(dir.path(), &files);

        let checks = derive_checks(&profile, &[PathBuf::from("firmware/src/main.cpp")]).unwrap();
        let required = required_ids(&checks);
        assert!(
            required.contains(&"firmware:cmake_configure".into()),
            "{required:?}"
        );
        assert!(
            required.contains(&"firmware:cmake_build".into()),
            "{required:?}"
        );
        assert!(
            required.contains(&"firmware:pio_run".into()),
            "{required:?}"
        );
        assert!(
            !required.contains(&"rust_check".into()),
            "a firmware change must never certify through only Rust checks: {required:?}"
        );
        for spec in &checks {
            assert_eq!(spec.cwd_rel, PathBuf::from("firmware"), "{spec:?}");
        }

        let checks = derive_checks(&profile, &[PathBuf::from("desktop/Program.cs")]).unwrap();
        assert_eq!(required_ids(&checks), vec!["desktop:dotnet_build"]);

        let checks = derive_checks(&profile, &[PathBuf::from("web/src/app.ts")]).unwrap();
        assert_eq!(required_ids(&checks), vec!["web:node_tsc"]);

        let checks = derive_checks(&profile, &[PathBuf::from("src/lib.rs")]).unwrap();
        assert!(required_ids(&checks).contains(&"rust_check".into()));
        assert!(checks
            .iter()
            .all(|spec| spec.cwd_rel.as_os_str().is_empty()));
    }

    #[test]
    fn a_node_source_change_derives_the_declared_test_script() {
        // The JS change is in src/, not in a test file. The component's
        // package.json declares `npm test`; that is the executable check and
        // must be REQUIRED. The old derivation returned an empty check set,
        // so every JS task was unverifiable by construction.
        let (dir, files) = fixture(&[
            ("package.json", "{\"scripts\":{\"test\":\"node --test\"}}"),
            ("src/app.js", "export const x = 1;\n"),
            ("test/app.test.js", "test('x', () => {});\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let checks =
            derive_checks_at_root(&profile, &[PathBuf::from("src/app.js")], dir.path()).unwrap();
        assert!(
            checks
                .iter()
                .any(|spec| spec.id.ends_with("node_test") && spec.required),
            "a declared test script must produce a required node_test: {checks:?}"
        );
    }

    #[test]
    fn a_src_change_runs_integration_tests_not_only_the_lib() {
        let (dir, files) = fixture(&[
            ("Cargo.toml", "[package]\nname = \"root\"\n"),
            (
                "src/lib.rs",
                "pub fn clamp_index(i: usize, n: usize) -> usize { i.min(n) }\n",
            ),
            (
                "tests/config.rs",
                "#[test]\nfn rejects_missing_port() {\n    assert!(true);\n}\n",
            ),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let checks =
            derive_checks_at_root(&profile, &[PathBuf::from("src/lib.rs")], dir.path()).unwrap();
        let integration = checks
            .iter()
            .find(|spec| spec.id.ends_with("rust_test_workspace"))
            .expect("src-only change must derive the integration test target");
        assert_eq!(
            integration.args,
            vec![
                std::ffi::OsString::from("test"),
                std::ffi::OsString::from("--tests")
            ]
        );
        assert!(integration.required);
    }

    #[test]
    fn inline_tests_in_a_plain_source_file_derive_executable_test_checks() {
        let (dir, files) = fixture(&[
            ("Cargo.toml", "[package]\nname = \"root\"\n"),
            (
                "src/lib.rs",
                "pub fn f() -> i64 { 7 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::f(), 7); }\n}\n",
            ),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let changed = vec![PathBuf::from("src/lib.rs")];
        // The rootless legacy entry point preserves its historical output.
        let rootless = derive_checks(&profile, &changed).unwrap();
        assert!(required_ids(&rootless).contains(&"rust_check".into()));
        assert!(
            !rootless
                .iter()
                .any(|spec| spec.id.starts_with("rust_test:tests::")),
            "no inline scan without an explicit root: {rootless:?}"
        );
        // The verifier's entry point discovers the contract compiler's exact
        // selector and derives a real `cargo test tests::adds` check.
        let rooted = derive_checks_at_root(&profile, &changed, dir.path()).unwrap();
        let test = rooted
            .iter()
            .find(|spec| spec.id == "rust_test:tests::adds")
            .unwrap_or_else(|| panic!("inline test check missing: {rooted:?}"));
        assert_eq!(test.kind, CheckKind::Test);
        assert!(test.required);
        assert_eq!(test.program, PathBuf::from("cargo"));
        assert_eq!(
            test.args,
            vec![
                std::ffi::OsString::from("test"),
                std::ffi::OsString::from("tests::adds")
            ]
        );
    }

    #[test]
    fn all_four_trees_survive_one_derivation_and_gate_completion() {
        let (dir, files) = mixed_repo();
        let profile = detect_project_profile(dir.path(), &files);
        let changed = vec![
            PathBuf::from("src/lib.rs"),
            PathBuf::from("firmware/src/main.cpp"),
            PathBuf::from("desktop/Program.cs"),
            PathBuf::from("web/src/app.ts"),
        ];
        let checks = derive_checks(&profile, &changed).unwrap();
        let required = required_ids(&checks);
        for family in [
            "rust_check",
            "firmware:cmake_configure",
            "firmware:cmake_build",
            "firmware:pio_run",
            "desktop:dotnet_build",
            "web:node_tsc",
        ] {
            assert!(
                required.contains(&family.to_string()),
                "family {family:?} was dropped: {required:?}"
            );
        }
        assert!(
            required.len() > 3,
            "no arbitrary MAX_CHECKS=3 truncation: {required:?}"
        );
        assert!(required.len() <= MAX_CHECKS);
        assert_eq!(
            required,
            vec![
                "desktop:dotnet_build",
                "firmware:cmake_configure",
                "firmware:cmake_build",
                "firmware:pio_run",
                "web:node_tsc",
                "rust_check",
            ],
            "deterministic component-then-family order"
        );

        let mirrors: Vec<Check> = checks.iter().map(mirror).collect();
        let all_pass: Vec<(String, bool)> = required.iter().map(|id| (id.clone(), true)).collect();
        assert_eq!(acceptance(&mirrors, &all_pass), Acceptance::Pass);

        for omitted in &required {
            let partial: Vec<(String, bool)> = all_pass
                .iter()
                .filter(|(id, _)| id != omitted)
                .cloned()
                .collect();
            assert_ne!(
                acceptance(&mirrors, &partial),
                Acceptance::Pass,
                "omitting {omitted} must not certify completion"
            );
        }
        let one_failed: Vec<(String, bool)> = all_pass
            .iter()
            .map(|(id, _)| (id.clone(), id != "desktop:dotnet_build"))
            .collect();
        assert_eq!(acceptance(&mirrors, &one_failed), Acceptance::Fail);
    }

    #[test]
    fn nested_components_resolve_ownership_by_longest_root_prefix() {
        let (dir, files) = fixture(&[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(top C)\n",
            ),
            ("main.cpp", "int main() { return 0; }\n"),
            (
                "vendor/thirdparty/CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(tp C)\n",
            ),
            ("vendor/thirdparty/src/x.cpp", "int x() { return 1; }\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        assert_eq!(roots(&profile), vec!["vendor/thirdparty", ""]);
        for order in [
            vec![
                PathBuf::from("vendor/thirdparty/src/x.cpp"),
                PathBuf::from("main.cpp"),
            ],
            vec![
                PathBuf::from("main.cpp"),
                PathBuf::from("vendor/thirdparty/src/x.cpp"),
            ],
        ] {
            let checks = derive_checks(&profile, &order).unwrap();
            let ids = ids(&checks);
            assert!(ids.contains(&"cmake_configure".into()), "{ids:?}");
            assert!(ids.contains(&"cmake_build".into()), "{ids:?}");
            assert!(
                ids.contains(&"vendor/thirdparty:cmake_configure".into()),
                "{ids:?}"
            );
            assert!(
                ids.contains(&"vendor/thirdparty:cmake_build".into()),
                "{ids:?}"
            );
            let root_checks: Vec<&CheckSpec> = checks
                .iter()
                .filter(|spec| spec.cwd_rel.as_os_str().is_empty())
                .collect();
            assert!(root_checks.iter().all(|spec| spec.affects.is_empty()));
            let nested: Vec<&CheckSpec> = checks
                .iter()
                .filter(|spec| spec.cwd_rel.as_path() == Path::new("vendor/thirdparty"))
                .collect();
            assert!(!nested.is_empty());
        }
    }

    #[test]
    fn caps_are_typed_errors_never_truncation() {
        let (dir, files) = mixed_repo();
        let profile = detect_project_profile(dir.path(), &files);
        let changed = vec![
            PathBuf::from("src/lib.rs"),
            PathBuf::from("firmware/src/main.cpp"),
            PathBuf::from("desktop/Program.cs"),
            PathBuf::from("web/src/app.ts"),
        ];
        let base = DerivationLimits::default();
        assert_eq!(base.max_checks, MAX_CHECKS);
        assert_eq!(base.max_simultaneous_jobs, MAX_SIMULTANEOUS_JOBS);

        let limits = DerivationLimits {
            max_checks: 1,
            ..base
        };
        match derive_checks_with_limits(&profile, &changed, limits) {
            Err(VerificationDerivationError::TooManyChecks { count, max }) => {
                assert!(count > max);
                assert_eq!(max, 1);
            }
            other => panic!("expected TooManyChecks, got {other:?}"),
        }
        let limits = DerivationLimits {
            max_total_wall: Duration::from_secs(1),
            ..base
        };
        assert!(matches!(
            derive_checks_with_limits(&profile, &changed, limits),
            Err(VerificationDerivationError::TotalWallBudgetExceeded { .. })
        ));

        let (dir, files) = fixture(&[
            ("CMakeLists.txt", "project(x C)\n"),
            ("src/main.c", "int main() { return 0; }\n"),
            ("tests/CMakeLists.txt", "add_test(NAME t COMMAND x)\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let limits = DerivationLimits {
            max_simultaneous_jobs: 0,
            ..base
        };
        match derive_checks_with_limits(&profile, &[PathBuf::from("src/main.c")], limits) {
            Err(VerificationDerivationError::TooManySimultaneousJobs { jobs, max }) => {
                assert!(jobs > max);
            }
            other => panic!("expected TooManySimultaneousJobs, got {other:?}"),
        }
    }

    #[test]
    fn identical_specs_dedupe_and_conflicting_ids_refuse() {
        let component = ProjectComponent {
            root: PathBuf::from("x"),
            languages: vec![LanguageFamily::C, LanguageFamily::Cpp],
            build_systems: vec![BuildSystem::CMake],
            toolchains: vec![Toolchain::CMake],
            targets: vec![],
            test_frameworks: vec![],
        };
        let profile = ProjectProfile {
            components: vec![component.clone(), component.clone()],
            manifest_refusals: Vec::new(),
        };
        let checks = derive_checks(&profile, &[PathBuf::from("x/main.c")]).unwrap();
        assert_eq!(
            required_ids(&checks),
            vec!["x:cmake_configure", "x:cmake_build"],
            "identical specs dedupe once: {checks:?}"
        );

        let mut conflicting = component.clone();
        conflicting.root = PathBuf::from("a:b");
        let mut other = component.clone();
        other.root = PathBuf::from("a_b");
        let profile = ProjectProfile {
            components: vec![conflicting, other],
            manifest_refusals: Vec::new(),
        };
        assert!(matches!(
            derive_checks(
                &profile,
                &[PathBuf::from("a:b/main.c"), PathBuf::from("a_b/main.c")]
            ),
            Err(VerificationDerivationError::ConflictingCheckId { .. })
        ));
    }

    #[test]
    fn embedded_markers_are_covered() {
        let (dir, files) = fixture(&[
            ("west.yml", "manifest:\n  projects: []\n"),
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20)\nfind_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\nproject(z)\n",
            ),
            ("src/main.c", "int main() { return 0; }\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let root = component_for(&profile, "");
        assert!(root.build_systems.contains(&BuildSystem::ZephyrWest));
        assert!(root.toolchains.contains(&Toolchain::West));
        assert!(root.toolchains.contains(&Toolchain::ZephyrSdk));
        assert!(root.targets.contains(&TargetProfile::Zephyr));
        let checks = derive_checks(&profile, &[PathBuf::from("src/main.c")]).unwrap();
        assert_eq!(required_ids(&checks), vec!["west_build"]);

        let (dir, files) = fixture(&[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\ninclude($ENV{IDF_PATH}/tools/cmake/project.cmake)\nproject(esp)\n",
            ),
            (
                "main/CMakeLists.txt",
                "idf_component_register(SRCS \"main.c\")\n",
            ),
            ("main/main.c", "void app_main(void) {}\n"),
            ("sdkconfig", "CONFIG_IDF_TARGET=\"esp32\"\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let root = component_for(&profile, "");
        assert!(root.build_systems.contains(&BuildSystem::EspIdf));
        assert!(root.toolchains.contains(&Toolchain::EspXtensa));
        assert!(root.targets.contains(&TargetProfile::Esp32));
        let checks = derive_checks(&profile, &[PathBuf::from("main/main.c")]).unwrap();
        assert_eq!(required_ids(&checks), vec!["idf_py_build"]);
        assert!(!ids(&checks).iter().any(|id| id.starts_with("cmake_")));

        let (dir, files) = fixture(&[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(arm C)\n",
            ),
            (
                "CMakePresets.json",
                "{\"configurePresets\":[{\"name\":\"dev\"}]}",
            ),
            (
                "cmake/arm-none-eabi.cmake",
                "set(CMAKE_C_COMPILER arm-none-eabi-gcc)\n",
            ),
            (
                "build/compile_commands.json",
                "[{\"command\":\"arm-none-eabi-gcc -c main.c\"}]",
            ),
            ("src/main.c", "int main() { return 0; }\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        let root = component_for(&profile, "");
        assert!(root.toolchains.contains(&Toolchain::ArmNoneEabi));
        assert!(root.targets.contains(&TargetProfile::ArmNoneEabi));
        assert!(root.build_systems.contains(&BuildSystem::CMake));
    }

    #[test]
    fn hostile_and_oversized_inputs_stay_bounded() {
        let (dir, files) = fixture(&[
            ("CMakeLists.txt", "project(x C)\n"),
            ("src/main.c", "int main() { return 0; }\n"),
        ]);
        let mut hostile = files.clone();
        hostile.push("../evil/CMakeLists.txt".to_string());
        hostile.push("/abs/CMakeLists.txt".to_string());
        let profile = detect_project_profile(dir.path(), &hostile);
        assert_eq!(roots(&profile), vec![""]);
        let checks = derive_checks(
            &profile,
            &[
                PathBuf::from("src/main.c"),
                PathBuf::from("../escape.c"),
                PathBuf::from("/abs/escape.c"),
                PathBuf::from("src/../../x.c"),
            ],
        )
        .unwrap();
        assert!(checks
            .iter()
            .all(|spec| spec.cwd_rel.as_os_str().is_empty()));
        assert!(checks
            .iter()
            .all(|spec| spec.affects.iter().all(|file| !file.contains(".."))));

        let (dir, files) = fixture(&[
            (
                "CMakeLists.txt",
                &format!("project(x)\n# arm-none-eabi\n{}", "y".repeat(300 * 1024)),
            ),
            ("src/main.c", "int main() { return 0; }\n"),
        ]);
        let profile = detect_project_profile(dir.path(), &files);
        assert!(profile
            .components
            .iter()
            .any(|c| c.build_systems.contains(&BuildSystem::CMake)));
        assert!(!profile
            .components
            .iter()
            .any(|c| c.toolchains.contains(&Toolchain::ArmNoneEabi)));
    }

    // --------------------------------------- manifest probe refusals (C)

    #[test]
    fn unreadable_package_json_refuses_instead_of_dropping_jest() {
        // A directory named `package.json` can never be read as a manifest;
        // the probe must surface that typed instead of silently acting as if
        // the manifest were absent (which would drop Jest detection).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("package.json")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.test.ts"), "test('x', () => {});\n").unwrap();
        let files = vec!["package.json".to_string(), "src/app.test.ts".to_string()];
        let profile = detect_project_profile(dir.path(), &files);
        let refusal = profile
            .manifest_refusals
            .first()
            .unwrap_or_else(|| panic!("the unreadable manifest must surface: {profile:?}"));
        assert_eq!(refusal.path, "package.json");
        assert_eq!(refusal.kind, "unreadable");
        // The Node marker facts still land, but the CONTENT-derived check set
        // cannot be certified: derivation refuses typed, never a smaller set.
        assert!(component_for(&profile, "")
            .build_systems
            .contains(&BuildSystem::Npm));
        let refused = derive_checks(&profile, &[PathBuf::from("src/app.test.ts")]);
        match refused {
            Err(VerificationDerivationError::ManifestProbeRefused { path, .. }) => {
                assert_eq!(path, "package.json");
            }
            other => panic!("unreadable content-deciding manifest must refuse: {other:?}"),
        }
    }

    #[test]
    fn oversized_package_json_refuses_instead_of_dropping_jest() {
        let (dir, mut files) = fixture(&[("src/app.test.ts", "test('x', () => {});\n")]);
        let oversized = format!(
            r#"{{"devDependencies":{{"jest":"30","pad":"{}"}}}}"#,
            "x".repeat(300 * 1024)
        );
        std::fs::write(dir.path().join("package.json"), oversized).unwrap();
        files.push("package.json".into());
        let profile = detect_project_profile(dir.path(), &files);
        assert_eq!(profile.manifest_refusals.len(), 1, "{profile:?}");
        assert_eq!(profile.manifest_refusals[0].kind, "oversized");
        assert!(
            !component_for(&profile, "")
                .test_frameworks
                .contains(&TestFramework::Jest),
            "oversized content is never read, so no framework may be claimed"
        );
        assert!(matches!(
            derive_checks(&profile, &[PathBuf::from("src/app.test.ts")]),
            Err(VerificationDerivationError::ManifestProbeRefused { .. })
        ));
    }

    #[test]
    fn unstable_generation_refuses_and_the_own_generation_detects_jest() {
        let (a, _files_a) = fixture(&[
            ("package.json", r#"{"devDependencies":{"jest":"30"}}"#),
            ("src/app.test.ts", "test('x', () => {});\n"),
        ]);
        let (b, files_b) = fixture(&[
            ("package.json", r#"{"devDependencies":{"jest":"30"}}"#),
            ("src/app.test.ts", "test('x', () => {});\n"),
        ]);
        // A generation admitted on A can never certify content under B: every
        // content probe refuses `unstable`, never absent.
        let foreign = CandidateGeneration::admit(a.path());
        let profile = detect_project_profile_at_generation(b.path(), &files_b, &foreign);
        assert!(
            !profile.manifest_refusals.is_empty(),
            "a mismatched generation must surface refusals: {profile:?}"
        );
        assert!(profile
            .manifest_refusals
            .iter()
            .any(|refusal| refusal.kind == "unstable"));
        assert!(matches!(
            derive_checks(&profile, &[PathBuf::from("src/app.test.ts")]),
            Err(VerificationDerivationError::ManifestProbeRefused { .. })
        ));

        // The own generation certifies the same tree: refusals stay empty and
        // the content-derived Jest detection lands.
        let own = CandidateGeneration::admit(b.path());
        let profile = detect_project_profile_at_generation(b.path(), &files_b, &own);
        assert!(profile.manifest_refusals.is_empty(), "{profile:?}");
        assert!(component_for(&profile, "")
            .test_frameworks
            .contains(&TestFramework::Jest));
        let checks = derive_checks(&profile, &[PathBuf::from("src/app.test.ts")]).unwrap();
        assert!(ids(&checks).contains(&"node_tsc".to_string()), "{checks:?}");
    }

    #[test]
    fn absent_manifest_stays_absent_without_a_refusal() {
        let (dir, mut files) = fixture(&[("src/index.ts", "export const x = 1;\n")]);
        // Listed by the inventory but genuinely absent on disk: no refusal,
        // and detection proceeds on the remaining evidence.
        files.push("package.json".into());
        let profile = detect_project_profile(dir.path(), &files);
        assert!(profile.manifest_refusals.is_empty(), "{profile:?}");
        let checks = derive_checks(&profile, &[PathBuf::from("src/index.ts")]).unwrap();
        assert!(ids(&checks).contains(&"node_tsc".to_string()), "{checks:?}");
    }

    #[test]
    fn legacy_project_type_conversion_is_single_component() {
        let profile = ProjectProfile::from(crate::ProjectType::Rust);
        let checks = derive_checks(&profile, &[PathBuf::from("src/a.rs")]).unwrap();
        assert!(required_ids(&checks).contains(&"rust_check".into()));
        let profile = ProjectProfile::from(crate::ProjectType::CMake);
        let checks = derive_checks(&profile, &[PathBuf::from("src/a.cpp")]).unwrap();
        assert!(required_ids(&checks).contains(&"cmake_build".into()));
        let profile = ProjectProfile::from(crate::ProjectType::Unknown);
        assert!(profile.components.is_empty());
        assert!(derive_checks(&profile, &[PathBuf::from("src/a.rs")])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn owning_component_is_public_data() {
        let (dir, files) = mixed_repo();
        let profile = detect_project_profile(dir.path(), &files);
        assert_eq!(
            profile
                .owning_component(Path::new("firmware/src/main.cpp"))
                .unwrap()
                .root,
            PathBuf::from("firmware")
        );
        assert_eq!(
            profile
                .owning_component(Path::new("web/scripts/build.ts"))
                .unwrap()
                .root,
            PathBuf::from("web")
        );
        assert_eq!(
            profile
                .owning_component(Path::new("README.md"))
                .unwrap()
                .root,
            PathBuf::from("")
        );
    }

    // ---------------------------------------------------------------- walk
    // Adversarial + complexity evidence for the ancestor-walk rewrites.

    fn walk_component(root: &str, build_systems: Vec<BuildSystem>) -> ProjectComponent {
        ProjectComponent {
            root: PathBuf::from(root),
            languages: Vec::new(),
            build_systems,
            toolchains: Vec::new(),
            targets: Vec::new(),
            test_frameworks: Vec::new(),
        }
    }

    /// The pre-rewrite full-scan `nearest_key`, kept verbatim as the
    /// correctness oracle for the ancestor walk.
    fn reference_nearest_key(
        components: &BTreeMap<String, ProjectComponent>,
        dir: &str,
    ) -> Option<String> {
        components
            .keys()
            .filter(|root| is_under(root, dir))
            .max_by_key(|root| root.matches('/').count() + usize::from(!root.is_empty()))
            .cloned()
    }

    /// The pre-rewrite nested-scan `detect_ctest`, kept verbatim (applied to
    /// its own map clone) as the correctness oracle for the ancestor walk.
    fn reference_detect_ctest(
        components: &mut BTreeMap<String, ProjectComponent>,
        files: &[String],
    ) {
        let mut additions: Vec<String> = Vec::new();
        for rel in files {
            let dir = dir_of(rel);
            let mut best: Option<(usize, String)> = None;
            for (key, component) in components.iter() {
                if !component.build_systems.contains(&BuildSystem::CMake) || !is_under(key, dir) {
                    continue;
                }
                let Some(relative) = relative_to(key, rel) else {
                    continue;
                };
                let discoverable = relative.starts_with("tests/")
                    || relative.starts_with("test/")
                    || relative.ends_with("CTestTestfile.cmake");
                if !discoverable {
                    continue;
                }
                let depth = key.matches('/').count() + usize::from(!key.is_empty());
                if best
                    .as_ref()
                    .is_none_or(|(best_depth, _)| depth > *best_depth)
                {
                    best = Some((depth, key.clone()));
                }
            }
            if let Some((_, key)) = best {
                additions.push(key);
            }
        }
        additions.sort();
        additions.dedup();
        for key in additions {
            add_test_framework(component_at(components, &key), TestFramework::CTest);
        }
    }

    #[test]
    fn ancestor_walk_matches_the_reference_scans_on_adversarial_profiles() {
        let map = || {
            let mut components: BTreeMap<String, ProjectComponent> = BTreeMap::new();
            components.insert(String::new(), walk_component("", vec![BuildSystem::Cargo]));
            components.insert("a".into(), walk_component("a", vec![BuildSystem::CMake]));
            components.insert(
                "a/b".into(),
                walk_component("a/b", vec![BuildSystem::CMake]),
            );
            components.insert(
                "a/b/c".into(),
                walk_component("a/b/c", vec![BuildSystem::Npm]),
            );
            components.insert("z".into(), walk_component("z", vec![BuildSystem::CMake]));
            components.insert("x/y".into(), walk_component("x/y", Vec::new()));
            components
        };
        let dirs = [
            "", "a", "a/b", "a/b/c", "a/b/c/d", "a/b/d", "a/bx", "a/x", "z", "z/w", "q", "x",
            "x/y", "x/y/z",
        ];
        let components = map();
        for dir in dirs {
            assert_eq!(
                nearest_key(&components, dir),
                reference_nearest_key(&components, dir),
                "nearest_key diverged for {dir:?}"
            );
        }
        let files: Vec<String> = [
            "a/b/tests/t.c",
            "a/b/c/tests/t.c",
            "a/b/c/CTestTestfile.cmake",
            "a/tests/deep/t.c",
            "a/test/x.c",
            "z/build.c",
            "z/tests/CTestTestfile.cmake",
            "x/y/tests/t.c",
            "a/b/src/wm.cmake",
            "a/bx/tests/t.c",
            "t.c",
        ]
        .iter()
        .map(|f| f.to_string())
        .collect();
        let mut walked = map();
        detect_ctest(&mut walked, &files);
        let mut reference = map();
        reference_detect_ctest(&mut reference, &files);
        assert_eq!(walked, reference, "detect_ctest diverged from the scan");
        // The discoverable deepest CMake ancestor is the one that gains CTest:
        // "a/b" (not "a") for its own tree, "z", and "a" (via test/ files).
        assert!(walked["a/b"]
            .test_frameworks
            .contains(&TestFramework::CTest));
        assert!(walked["z"].test_frameworks.contains(&TestFramework::CTest));
        assert!(walked["a"].test_frameworks.contains(&TestFramework::CTest));
        assert!(
            !walked["a/b/c"]
                .test_frameworks
                .contains(&TestFramework::CTest),
            "a non-CMake component never gains CTest"
        );
    }

    #[test]
    fn profile_probe_work_scales_with_depth_not_component_count() {
        // 10k files across `component_count` CMake components (the realistic
        // monorepo shape): the ancestor walk probes a constant number of
        // dirs per file (its own dir, then the component root); the old scan
        // probed every component per file. The instrumented counter proves
        // the bound in work units, with no timing.
        let run = |component_count: usize| -> (u64, Vec<String>) {
            let mut components: BTreeMap<String, ProjectComponent> = BTreeMap::new();
            for i in 0..component_count {
                let root = format!("pkg{i:04}");
                components.insert(
                    root.clone(),
                    walk_component(&root, vec![BuildSystem::CMake]),
                );
            }
            let files: Vec<String> = (0..10_000)
                .map(|f| format!("pkg{:04}/tests/t{f:05}.c", f % component_count))
                .collect();
            PROFILE_PROBES.with(|counter| counter.set(0));
            detect_ctest(&mut components, &files);
            let probes = PROFILE_PROBES.with(std::cell::Cell::get);
            assert_eq!(
                components
                    .values()
                    .filter(|c| c.test_frameworks.contains(&TestFramework::CTest))
                    .count(),
                component_count,
                "every component must still be detected"
            );
            (probes, files)
        };
        let (small_probes, files) = run(40);
        let (large_probes, _) = run(400);
        assert_eq!(
            small_probes, large_probes,
            "walk probes must be independent of the component count"
        );
        assert!(
            large_probes <= 2 * files.len() as u64 + 4,
            "bounded by files x (depth + 1): {large_probes} probes"
        );
        let old_work_units = files.len() as u64 * 400;
        assert!(
            old_work_units >= 50 * large_probes,
            "old scan {old_work_units} work units vs walk {large_probes}: \
             the rewrite must remove at least 50x of the probe work"
        );
    }

    #[test]
    fn owner_indices_match_owning_component_for_every_adversarial_path() {
        // Unsorted depths, a duplicated root (first wins), the root
        // component, and a non-ancestor sibling: the batch index must agree
        // with the public per-file scan for ALL of them.
        let mut first = walk_component("a/b", vec![BuildSystem::CMake]);
        first.languages = vec![LanguageFamily::C];
        let mut duplicate = walk_component("a/b", vec![BuildSystem::CMake]);
        duplicate.languages = vec![LanguageFamily::Cpp];
        let profile = ProjectProfile {
            components: vec![
                walk_component("", vec![BuildSystem::Cargo]),
                walk_component("a", vec![BuildSystem::Cargo]),
                first,
                duplicate,
                walk_component("a/b/c", vec![BuildSystem::Npm]),
                walk_component("z", Vec::new()),
            ],
            manifest_refusals: Vec::new(),
        };
        let files = [
            "",
            "a",
            "a/b",
            "a/b/c",
            "a/b/c/d.rs",
            "a/b/d.rs",
            "a/bx/d.rs",
            "a/x.rs",
            "z",
            "z/y",
            "q",
            "a//b/c.rs",
            "a/b/./c.rs",
            "a/./b/c.rs",
            "trailing/",
            "a/b/",
        ];
        let batch = profile.owner_indices(files.iter().map(Path::new));
        for (file, got) in files.iter().zip(batch) {
            let expected = profile
                .owning_component(Path::new(file))
                .map(|c| c.root.clone());
            let got = got.map(|index| profile.components[index].root.clone());
            assert_eq!(got, expected, "owner diverged for {file:?}");
        }
        // The duplicated root resolves to its FIRST component on both paths.
        let a_b = profile.owner_indices([Path::new("a/b/x.rs")])[0].unwrap();
        assert_eq!(a_b, 2, "first component at a depth tie wins");
    }

    #[test]
    fn hoisted_owner_batches_keep_conflict_and_dedupe_semantics() {
        // Same-id specs still conflict typed; identical specs still collapse
        // (via the existing duplicate-component fixture), now through the
        // id map instead of the O(k^2) first-match scan.
        let component = ProjectComponent {
            root: PathBuf::from("x"),
            languages: vec![LanguageFamily::C, LanguageFamily::Cpp],
            build_systems: vec![BuildSystem::CMake],
            toolchains: vec![Toolchain::CMake],
            targets: vec![],
            test_frameworks: vec![],
        };
        let profile = ProjectProfile {
            components: vec![component.clone(), component],
            manifest_refusals: Vec::new(),
        };
        let checks = derive_checks(&profile, &[PathBuf::from("x/main.c")]).unwrap();
        assert_eq!(
            required_ids(&checks),
            vec!["x:cmake_configure", "x:cmake_build"]
        );
    }
}
