//! Pixel-regression runner tests: the rendered panel corpus must pass
//! deterministically, and every failure mode of the runner is exercised
//! adversarially with synthetic corpora (missing/corrupt files, hostile
//! geometry, invisible overlays, tolerance abuse, bounded reporting).
//!
//! There is no committed screenshot corpus: the frozen 16-case audit matrix
//! is RENDERED at test time into a temp directory at the bounded
//! `PANEL_WIDTH`x`PANEL_HEIGHT` size, so repository weight stays flat while
//! the exact same audit contract (zero-pixel diff outside mask+tolerance,
//! visible overlay inside the mask) is exercised on real panel geometry.

use std::path::Path;

use faktor_tests_visual::runner::run_pixel_regression;
use faktor_tests_visual::{
    compose_plus_overlay, fixture_screenshot, render_panel, BrandingMask, PANEL_HEIGHT, PANEL_WIDTH,
};
use image::{Rgba, RgbaImage};

/// An overlay with the branding `+` drawn inside the mask (like the real
/// corpus), so a "good" entry passes the visibility requirement.
fn branded_overlay(base: &RgbaImage, mask: (u32, u32, u32, u32)) -> RgbaImage {
    compose_plus_overlay(
        base,
        BrandingMask {
            x: mask.0,
            y: mask.1,
            width: mask.2,
            height: mask.3,
        },
        Rgba([255, 255, 255, 255]),
    )
}

fn entry(
    name: &str,
    baseline: &str,
    overlay: &str,
    mask: (u32, u32, u32, u32),
    tolerance: (u32, u32, u32, u32),
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "baseline": baseline,
        "overlay": overlay,
        "mask": {"x": mask.0, "y": mask.1, "width": mask.2, "height": mask.3},
        "tolerance": {"x": tolerance.0, "y": tolerance.1, "width": tolerance.2, "height": tolerance.3},
    })
}

fn write_manifest(dir: &Path, entries: &[serde_json::Value]) {
    let json = serde_json::to_string_pretty(&serde_json::Value::Array(entries.to_vec())).unwrap();
    std::fs::write(dir.join("manifest.json"), json).unwrap();
}

fn write_png(dir: &Path, name: &str, img: &RgbaImage) {
    img.save(dir.join(name)).unwrap();
}

/// The frozen 16-case audit matrix. Every case renders a distinct bounded
/// panel at test time; the mask/tolerance geometry stays inside the panel.
struct PanelCase {
    name: &'static str,
    seed: u8,
    mask: (u32, u32, u32, u32),
    tolerance: (u32, u32, u32, u32),
}

const PANEL_CASES: [PanelCase; 16] = [
    PanelCase {
        name: "vscode-dark-empty-chat",
        seed: 10,
        mask: (240, 8, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-streaming",
        seed: 11,
        mask: (240, 36, 24, 24),
        tolerance: (0, 182, 320, 18),
    },
    PanelCase {
        name: "vscode-dark-tool-call",
        seed: 12,
        mask: (180, 80, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-diff",
        seed: 13,
        mask: (120, 120, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-permission",
        seed: 14,
        mask: (120, 60, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-settings",
        seed: 15,
        mask: (16, 160, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-model-selector",
        seed: 16,
        mask: (100, 50, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-agent-manager",
        seed: 17,
        mask: (240, 100, 24, 24),
        tolerance: (0, 182, 320, 18),
    },
    PanelCase {
        name: "vscode-dark-background-agent",
        seed: 18,
        mask: (200, 140, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-history",
        seed: 19,
        mask: (220, 150, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-error",
        seed: 20,
        mask: (100, 90, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-dark-compaction",
        seed: 21,
        mask: (200, 40, 24, 24),
        tolerance: (0, 0, 320, 2),
    },
    PanelCase {
        name: "vscode-light-empty-chat",
        seed: 22,
        mask: (240, 8, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "vscode-light-settings",
        seed: 23,
        mask: (16, 160, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "jetbrains-dark-empty-chat",
        seed: 24,
        mask: (260, 10, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
    PanelCase {
        name: "jetbrains-light-empty-chat",
        seed: 25,
        mask: (260, 10, 24, 24),
        tolerance: (0, 0, 0, 0),
    },
];

/// Render the frozen matrix at test time: for each case a distinct bounded
/// panel, its branded `+` overlay, and the manifest entry locking the mask
/// and tolerance zone.
fn render_corpus(dir: &Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for case in &PANEL_CASES {
        let baseline = render_panel(PANEL_WIDTH, PANEL_HEIGHT, case.seed);
        let mask = BrandingMask {
            x: case.mask.0,
            y: case.mask.1,
            width: case.mask.2,
            height: case.mask.3,
        };
        let overlay = compose_plus_overlay(&baseline, mask, Rgba([255, 255, 255, 255]));
        write_png(dir, &format!("{}.png", case.name), &baseline);
        write_png(dir, &format!("{}-overlay.png", case.name), &overlay);
        entries.push(entry(
            case.name,
            &format!("{}.png", case.name),
            &format!("{}-overlay.png", case.name),
            case.mask,
            case.tolerance,
        ));
    }
    write_manifest(dir, &entries);
    entries
}

/// The frozen corpus: all 16 rendered manifest entries pass the audit
/// contract, the matrix size is locked, entries are unique, and every
/// baseline is a distinct panel.
#[test]
fn pixel_regression_over_rendered_panels() {
    let dir = tempfile::tempdir().unwrap();
    let entries = render_corpus(dir.path());
    let passed = run_pixel_regression(dir.path())
        .unwrap_or_else(|e| panic!("rendered panels must pass the audit contract: {e}"));
    let manifest: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(passed, manifest.len(), "every manifest entry passes");
    assert_eq!(passed, 16, "the full audit matrix is frozen at 16 cases");
    assert_eq!(entries.len(), 16);
    let mut names = std::collections::BTreeSet::new();
    for e in &manifest {
        let name = e["name"].as_str().unwrap();
        assert!(names.insert(name), "duplicate manifest entry {name}");
        assert!(
            dir.path().join(e["baseline"].as_str().unwrap()).exists(),
            "baseline {} missing",
            e["baseline"]
        );
        assert!(
            dir.path().join(e["overlay"].as_str().unwrap()).exists(),
            "overlay {} missing",
            e["overlay"]
        );
    }
    assert_eq!(names.len(), 16);
    let mut hashes = std::collections::BTreeSet::new();
    for e in &manifest {
        let bytes = std::fs::read(dir.path().join(e["baseline"].as_str().unwrap())).unwrap();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        assert!(
            hashes.insert(hash),
            "{} duplicates another baseline",
            e["name"]
        );
    }
    assert_eq!(hashes.len(), 16, "every case must be a distinct panel");
}

#[test]
fn runner_missing_manifest_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("manifest"), "{err}");
}

#[test]
fn runner_malformed_manifest_is_err() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.json"), "{not json").unwrap();
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("manifest"), "{err}");
}

#[test]
fn runner_missing_referenced_png_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let img = fixture_screenshot(64, 64, 9);
    write_png(dir.path(), "a.png", &img);
    // a-overlay.png is never written.
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("a"), "{err}");
    assert!(err.contains("a-overlay.png"), "{err}");
}

#[test]
fn runner_corrupt_png_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let good = fixture_screenshot(64, 64, 1);
    write_png(dir.path(), "a.png", &good);
    write_png(
        dir.path(),
        "a-overlay.png",
        &branded_overlay(&good, (10, 10, 12, 12)),
    );
    std::fs::write(dir.path().join("b.png"), b"not a png at all").unwrap();
    write_png(dir.path(), "b-overlay.png", &good);
    write_manifest(
        dir.path(),
        &[
            entry(
                "a",
                "a.png",
                "a-overlay.png",
                (10, 10, 12, 12),
                (0, 0, 0, 0),
            ),
            entry(
                "b",
                "b.png",
                "b-overlay.png",
                (10, 10, 12, 12),
                (0, 0, 0, 0),
            ),
        ],
    );
    // The first entry passes; the corrupt one must be named.
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("b"), "{err}");
    assert!(err.contains("cannot decode"), "{err}");
}

#[test]
fn runner_baseline_overlay_size_mismatch_is_err() {
    let dir = tempfile::tempdir().unwrap();
    write_png(dir.path(), "a.png", &fixture_screenshot(64, 64, 8));
    write_png(dir.path(), "a-overlay.png", &fixture_screenshot(63, 64, 8));
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("a"), "{err}");
    assert!(err.contains("mismatch"), "{err}");
}

#[test]
fn runner_outside_mask_change_names_entry_and_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let base = fixture_screenshot(64, 64, 2);
    let mut overlay = base.clone();
    overlay.put_pixel(0, 0, Rgba([255, 0, 0, 255])); // outside the mask
    overlay.put_pixel(5, 5, Rgba([0, 255, 0, 255])); // outside the mask
    overlay.put_pixel(15, 15, Rgba([0, 0, 255, 255])); // inside the mask
    write_png(dir.path(), "a.png", &base);
    write_png(dir.path(), "a-overlay.png", &overlay);
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("a"), "{err}");
    assert!(err.contains("(0, 0)"), "{err}");
    assert!(err.contains("(5, 5)"), "{err}");
}

#[test]
fn runner_offending_pixel_list_is_bounded_to_first_20() {
    let dir = tempfile::tempdir().unwrap();
    let base = fixture_screenshot(64, 64, 3);
    let mut overlay = base.clone();
    for x in 0..25 {
        overlay.put_pixel(x, 0, Rgba([255, 0, 0, 255]));
    }
    write_png(dir.path(), "a.png", &base);
    write_png(dir.path(), "a-overlay.png", &overlay);
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (30, 30, 8, 8),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("25 pixels changed"), "{err}");
    assert!(
        err.contains("(0, 0)"),
        "first pixel must be reported: {err}"
    );
    assert!(
        !err.contains("(24, 0)"),
        "reporting must stop at 20 pixels: {err}"
    );
}

#[test]
fn runner_zero_size_mask_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let img = fixture_screenshot(64, 64, 4);
    // Identical baseline/overlay: a zero-size mask has no visibility
    // requirement, so the entry passes with zero changed pixels.
    write_png(dir.path(), "a.png", &img);
    write_png(dir.path(), "a-overlay.png", &img);
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        )],
    );
    assert_eq!(run_pixel_regression(dir.path()).unwrap(), 1);
}

#[test]
fn runner_invisible_overlay_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let img = fixture_screenshot(64, 64, 5);
    // A non-empty mask with zero changed pixels inside: the overlay is not
    // actually visible — the audit contract is violated.
    write_png(dir.path(), "a.png", &img);
    write_png(dir.path(), "a-overlay.png", &img);
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("a"), "{err}");
    assert!(err.contains("invisible"), "{err}");
}

#[test]
fn runner_tolerance_absorbs_outside_changes() {
    let dir = tempfile::tempdir().unwrap();
    let base = fixture_screenshot(64, 64, 6);
    let mut overlay = branded_overlay(&base, (10, 10, 12, 12));
    overlay.put_pixel(2, 2, Rgba([255, 255, 0, 255]));
    write_png(dir.path(), "a.png", &base);
    write_png(dir.path(), "a-overlay.png", &overlay);
    // The tolerance zone covers the changed pixel: the entry passes.
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 16, 16),
        )],
    );
    assert_eq!(run_pixel_regression(dir.path()).unwrap(), 1);
    // The exact same corpus with an empty tolerance zone fails: the
    // absorption is the tolerance's doing, not a runner bug.
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("(2, 2)"), "{err}");
}

#[test]
fn runner_mask_out_of_bounds_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let img = fixture_screenshot(64, 64, 7);
    write_png(dir.path(), "a.png", &img);
    write_png(dir.path(), "a-overlay.png", &img);
    // Right edge beyond the 64px width.
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (60, 0, 24, 24),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("mask"), "{err}");
    assert!(err.contains("out of bounds"), "{err}");
    // u32 overflow in x+width must be caught by checked arithmetic, not a
    // panic.
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (u32::MAX - 5, 0, 24, 24),
            (0, 0, 0, 0),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("out of bounds"), "{err}");
}

#[test]
fn runner_tolerance_out_of_bounds_is_err() {
    let dir = tempfile::tempdir().unwrap();
    let img = fixture_screenshot(64, 64, 10);
    write_png(dir.path(), "a.png", &img);
    write_png(dir.path(), "a-overlay.png", &img);
    write_manifest(
        dir.path(),
        &[entry(
            "a",
            "a.png",
            "a-overlay.png",
            (10, 10, 12, 12),
            (0, 60, 64, 24),
        )],
    );
    let err = run_pixel_regression(dir.path()).unwrap_err();
    assert!(err.contains("tolerance"), "{err}");
    assert!(err.contains("out of bounds"), "{err}");
}
