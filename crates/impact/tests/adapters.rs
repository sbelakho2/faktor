use std::fs;
use std::path::Path;
use std::sync::Arc;

use faktor_impact::{
    AffectedOptions, ChangedTarget, EdgeKind, GraphBuilder, NodeKind, Revision, TopologyOptions,
};
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn build(root: &Path) -> faktor_impact::Graph {
    GraphBuilder::new(root, Revision::default())
        .unwrap()
        .build()
        .unwrap()
}

fn build_parser(root: &Path) -> (faktor_impact::Graph, faktor_impact::BuildReport) {
    let mut builder =
        GraphBuilder::new_with_topology(root, Revision::default(), TopologyOptions::parser_only())
            .unwrap();
    let report = builder.report().clone();
    (builder.build().unwrap(), report)
}

fn has_edge(graph: &faktor_impact::Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    graph
        .edges
        .iter()
        .any(|edge| edge.from == from && edge.to == to && edge.kind == kind)
}

fn has_node(graph: &faktor_impact::Graph, id: &str, kind: NodeKind) -> bool {
    graph
        .nodes
        .iter()
        .any(|node| node.id == id && node.kind == kind)
}

#[test]
fn npm_workspaces_compose_packages_and_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"fixture-root","private":true,"workspaces":["packages/*"]}"#,
    );
    write(
        root,
        "packages/lib/package.json",
        r#"{"name":"@fixture/lib","version":"1.0.0"}"#,
    );
    write(
        root,
        "packages/lib/index.js",
        "export function add(a, b) { return a + b; }\n\ntest('add works', () => { add(1, 2); });\n",
    );
    write(
        root,
        "packages/app/package.json",
        r#"{"name":"@fixture/app","version":"1.0.0","dependencies":{"@fixture/lib":"1.0.0"}}"#,
    );
    write(
        root,
        "packages/app/main.js",
        "import { add } from '@fixture/lib';\nexport const answer = add(1, 2);\n",
    );
    let graph = build(root);
    assert!(has_node(&graph, "package:@fixture/lib", NodeKind::Package));
    assert!(has_node(&graph, "package:@fixture/app", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:@fixture/app",
        "package:@fixture/lib",
        EdgeKind::DependsOn
    ));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Test
            && node.crate_name.as_deref() == Some("@fixture/lib")));
    assert!(graph
        .edges
        .iter()
        .any(|edge| edge.kind == EdgeKind::Tests && edge.to == "package:@fixture/lib"));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file("packages/lib/index.js")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "add works"));
}

#[test]
fn cargo_and_npm_adapters_compose_in_one_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"rust-lib\"]\n",
    );
    write(
        root,
        "rust-lib/Cargo.toml",
        "[package]\nname = \"rust-lib\"\nversion = \"0.1.0\"\n",
    );
    write(root, "rust-lib/src/lib.rs", "pub fn value() -> u32 { 1 }\n");
    write(
        root,
        "package.json",
        r#"{"name":"web-root","private":true,"workspaces":["ui"]}"#,
    );
    write(
        root,
        "ui/package.json",
        r#"{"name":"@fixture/ui","version":"1.0.0","dependencies":{"web-root":"1.0.0"}}"#,
    );
    write(root, "ui/index.js", "export const ui = 1;\n");
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let report = builder.report().clone();
    assert!(
        report.adapters.iter().any(|adapter| adapter == "cargo")
            && report.adapters.iter().any(|adapter| adapter == "npm"),
        "both adapters must contribute: {report:?}"
    );
    let graph = builder.build().unwrap();
    assert!(has_node(&graph, "crate:rust-lib", NodeKind::Crate));
    assert!(has_node(&graph, "package:web-root", NodeKind::Package));
    assert!(has_node(&graph, "package:@fixture/ui", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:@fixture/ui",
        "package:web-root",
        EdgeKind::DependsOn
    ));
}

#[test]
fn gradle_settings_and_project_dependencies_are_derived() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "settings.gradle", "include ':lib', ':app'\n");
    write(
        root,
        "lib/build.gradle",
        "plugins { id 'java' }\ndependencies { testImplementation 'junit:junit:4.13.2' }\n",
    );
    write(
        root,
        "app/build.gradle",
        "dependencies { implementation project(':lib') }\n",
    );
    write(
        root,
        "lib/src/main/java/com/example/Lib.java",
        "package com.example;\npublic class Lib { public static int add(int a, int b) { return a + b; } }\n",
    );
    write(
        root,
        "lib/src/test/java/com/example/LibTest.java",
        "package com.example;\nimport org.junit.Test;\npublic class LibTest {\n  @Test\n  public void adds() { Lib.add(1, 2); }\n}\n",
    );
    write(
        root,
        "app/src/main/java/com/example/App.java",
        "package com.example;\npublic class App { int value = Lib.add(1, 2); }\n",
    );
    let graph = build(root);
    assert!(has_node(&graph, "package:lib", NodeKind::Package));
    assert!(has_node(&graph, "package:app", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:app",
        "package:lib",
        EdgeKind::DependsOn
    ));
    assert!(has_edge(
        &graph,
        "package:lib",
        "file:lib/src/test/java/com/example/LibTest.java",
        EdgeKind::TestTarget
    ));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file(
                "lib/src/main/java/com/example/Lib.java",
            )],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "adds"));
}

#[test]
fn maven_modules_and_test_roots_are_derived() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pom.xml",
        r#"<project><groupId>com.example</groupId><artifactId>root</artifactId><packaging>pom</packaging><modules><module>lib</module><module>app</module></modules></project>"#,
    );
    write(
        root,
        "lib/pom.xml",
        r#"<project><groupId>com.example</groupId><artifactId>lib</artifactId></project>"#,
    );
    write(
        root,
        "app/pom.xml",
        r#"<project><groupId>com.example</groupId><artifactId>app</artifactId><dependencies><dependency><groupId>com.example</groupId><artifactId>lib</artifactId></dependency></dependencies></project>"#,
    );
    write(
        root,
        "lib/src/main/java/com/example/Lib.java",
        "package com.example;\npublic class Lib { public static int add(int a, int b) { return a + b; } }\n",
    );
    write(
        root,
        "lib/src/test/java/com/example/LibTest.java",
        "package com.example;\nimport org.junit.Test;\npublic class LibTest {\n  @Test\n  public void adds() { Lib.add(1, 2); }\n}\n",
    );
    write(
        root,
        "app/src/main/java/com/example/App.java",
        "package com.example;\npublic class App { int value = Lib.add(1, 2); }\n",
    );
    let graph = build(root);
    assert!(has_node(&graph, "package:lib", NodeKind::Package));
    assert!(has_node(&graph, "package:app", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:app",
        "package:lib",
        EdgeKind::DependsOn
    ));
    assert!(graph.files.iter().any(|file| file
        .path
        .ends_with("lib/src/test/java/com/example/LibTest.java")
        && file.test_target));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file(
                "lib/src/main/java/com/example/Lib.java",
            )],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "adds"));
}

#[test]
fn python_package_metadata_and_tests_are_derived() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pyproject.toml",
        r#"[project]
name = "fixture-pkg"
version = "0.1.0"
dependencies = ["requests>=2", "pydantic"]
"#,
    );
    write(
        root,
        "fixture_pkg/module.py",
        "def add(a, b):\n    return a + b\n",
    );
    write(
        root,
        "tests/test_module.py",
        "from fixture_pkg.module import add\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    );
    let graph = build(root);
    assert!(has_node(&graph, "package:fixture-pkg", NodeKind::Package));
    assert!(has_node(&graph, "package:requests", NodeKind::Package));
    assert!(has_node(&graph, "package:pydantic", NodeKind::Package));
    assert!(graph
        .edges
        .iter()
        .any(|edge| edge.from == "package:fixture-pkg" && edge.kind == EdgeKind::DependsOn));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Test && node.name == "test_add"));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file("fixture_pkg/module.py")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "test_add"));
}

#[test]
fn renamed_cargo_dependencies_keep_the_actual_package_edge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"actual\", \"app\"]\n",
    );
    write(
        root,
        "actual/Cargo.toml",
        "[package]\nname = \"actual\"\nversion = \"0.1.0\"\n",
    );
    write(root, "actual/src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nalias = { package = \"actual\", path = \"../actual\" }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "pub fn value() -> u32 { actual::answer() }\n",
    );
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let workspace = builder.workspace().clone();
    assert!(workspace.crates.contains_key("actual"));
    assert!(workspace.crates.contains_key("app"));
    assert!(workspace.crates["app"].deps.contains("actual"));
    assert!(!workspace.crates["app"].deps.contains("alias"));
    assert_eq!(
        workspace
            .workspace_members
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["actual".to_string(), "app".to_string()]
    );
    let graph = builder.build().unwrap();
    assert!(has_edge(
        &graph,
        "crate:app",
        "crate:actual",
        EdgeKind::DependsOn
    ));
}

#[test]
fn workspace_default_members_and_targets_are_resolved_by_cargo_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\ndefault-members = [\"a\"]\n",
    );
    write(
        root,
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(root, "a/src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    write(
        root,
        "a/tests/integration.rs",
        "#[test]\nfn works() { assert_eq!(a::a(), 1); }\n",
    );
    write(
        root,
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
    );
    write(root, "b/src/lib.rs", "pub fn b() -> u32 { 2 }\n");
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let workspace = builder.workspace();
    assert_eq!(
        workspace
            .default_members
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["a".to_string()]
    );
    assert!(workspace.crates["a"].kind == faktor_impact::TopologyUnitKind::Crate);
    assert!(workspace.crates["a"]
        .test_targets
        .iter()
        .any(|target| target.ends_with("tests/integration.rs")));
    let report = builder.report().clone();
    assert!(
        !report.degraded,
        "cargo metadata must not degrade: {report:?}"
    );
    assert!(report.adapters.iter().any(|adapter| adapter == "cargo"));
    let graph = builder.build().unwrap();
    assert!(graph
        .files
        .iter()
        .any(|file| file.path == "a/tests/integration.rs" && file.test_target));
}

#[test]
fn cargo_metadata_runs_once_per_revision_through_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\n");
    write(
        root,
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(root, "a/src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let cas_root = tempfile::tempdir().unwrap();
    let supervisor =
        ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(cas_root.path().join("cas"))));
    let revision = Revision {
        commit: Some(format!("metadata-cache-{}", std::process::id())),
        tree: None,
        dirty: false,
    };
    let options = TopologyOptions {
        supervisor: Some(Arc::clone(&supervisor)),
        ..TopologyOptions::default()
    };
    let count_metadata = |supervisor: &Arc<ProcessSupervisor>| {
        supervisor
            .recent_spawns()
            .into_iter()
            .filter(|spawn| spawn.argv.starts_with("cargo metadata --no-deps"))
            .count()
    };
    let before = count_metadata(&supervisor);
    let mut first =
        GraphBuilder::new_with_topology(root, revision.clone(), options.clone()).unwrap();
    first.build().unwrap();
    let after_first = count_metadata(&supervisor);
    assert_eq!(
        after_first,
        before + 1,
        "the first build must spawn exactly one supervised cargo metadata"
    );
    let mut second = GraphBuilder::new_with_topology(root, revision, options).unwrap();
    second.build().unwrap();
    assert_eq!(
        count_metadata(&supervisor),
        after_first,
        "the same revision must reuse the cached topology"
    );
    let cargo = supervisor
        .recent_spawns()
        .into_iter()
        .find(|spawn| spawn.argv.starts_with("cargo metadata --no-deps"))
        .expect("cargo metadata timeline row");
    assert!(
        cargo.exited_ms.is_some(),
        "the supervised cargo child must be reaped, not orphaned: {cargo:?}"
    );
    assert_eq!(supervisor.kill_all_for(ProcessOwner::Daemon).len(), 0);
}

#[test]
fn documented_parser_fallback_is_typed_and_degrades_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"actual\", \"app\"]\n",
    );
    write(
        root,
        "actual/Cargo.toml",
        "[package]\nname = \"actual\"\nversion = \"0.1.0\"\n",
    );
    write(root, "actual/src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nalias = { package = \"actual\", path = \"../actual\" }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "pub fn value() -> u32 { actual::answer() }\n",
    );
    let (graph, report) = build_parser(root);
    assert!(report.degraded, "parser fallback must be marked degraded");
    assert!(
        report
            .degraded_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("bounded manifest fallback")),
        "{report:?}"
    );
    assert!(has_edge(
        &graph,
        "crate:app",
        "crate:actual",
        EdgeKind::DependsOn
    ));
}

#[test]
fn npm_native_lock_graph_preserves_workspaces_and_is_marked_native() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"lock-root","private":true,"workspaces":["packages/*"]}"#,
    );
    write(
        root,
        "package-lock.json",
        r#"{
          "name": "lock-root", "lockfileVersion": 3, "requires": true,
          "packages": {
            "": { "name": "lock-root", "workspaces": ["packages/*"] },
            "node_modules/@lock/lib": { "resolved": "packages/lib", "link": true },
            "node_modules/@lock/app": { "resolved": "packages/app", "link": true },
            "packages/lib": { "name": "@lock/lib", "version": "1.0.0" },
            "packages/app": {
              "name": "@lock/app", "version": "1.0.0",
              "dependencies": { "@lock/lib": "1.0.0", "left-pad": "^1.3.0" },
              "devDependencies": { "vitest": "^1.0.0" }
            }
          }
        }"#,
    );
    write(
        root,
        "packages/lib/index.js",
        "export const add = (a,b) => a+b;\n",
    );
    write(
        root,
        "packages/lib/test/add.test.js",
        "import { add } from '../index.js';\ntest('add', () => add(1,2));\n",
    );
    write(root, "packages/app/index.js", "export const x = 1;\n");
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let report = builder.report().clone();
    let npm = report
        .adapter_outcomes
        .iter()
        .find(|outcome| outcome.adapter == faktor_impact::AdapterKind::Npm)
        .expect("npm outcome");
    assert!(
        !npm.degraded && npm.mode == faktor_impact::AdapterMode::NativeMetadata,
        "the package-lock graph must serve natively: {npm:?}"
    );
    let graph = builder.build().unwrap();
    assert!(has_edge(
        &graph,
        "package:@lock/app",
        "package:@lock/lib",
        EdgeKind::DependsOn
    ));
    assert!(has_edge(
        &graph,
        "package:@lock/app",
        "package:left-pad",
        EdgeKind::DependsOn
    ));
    assert!(graph
        .files
        .iter()
        .any(|file| file.path == "packages/lib/test/add.test.js" && file.test_target));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file("packages/lib/index.js")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "add"));
}

#[test]
fn npm_lock_hostile_keys_are_refused_and_structural_fallback_is_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"hostile-lock","workspaces":["packages/*"]}"#,
    );
    write(
        root,
        "package-lock.json",
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "name": "hostile-lock" },
            "../escape": { "name": "escape" },
            "packages/ok": { "name": "ok", "dependencies": { "../nope": "1" } }
          }
        }"#,
    );
    write(root, "packages/ok/package.json", r#"{"name":"ok"}"#);
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let graph = builder.build().unwrap();
    assert!(graph.nodes.iter().all(|node| node.name != "escape"));
    let report = builder.report().clone();
    assert!(report
        .skipped
        .iter()
        .any(|skip| skip.contains("escapes") || skip.contains("missing")));
    assert!(!graph.nodes.iter().any(|node| node.name == "../nope"));

    let options = TopologyOptions {
        native_metadata: false,
        ..TopologyOptions::default()
    };
    let structural = GraphBuilder::new_with_topology(root, Revision::default(), options).unwrap();
    let report = structural.report().clone();
    let npm = report
        .adapter_outcomes
        .iter()
        .find(|outcome| outcome.adapter == faktor_impact::AdapterKind::Npm)
        .expect("npm fallback outcome");
    assert!(
        npm.degraded,
        "structural npm inference is degraded: {npm:?}"
    );
    assert!(report.degraded);
}

#[test]
fn maven_recursive_poms_resolve_parents_properties_and_nested_modules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    write(
        &root,
        "pom.xml",
        r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId><artifactId>root</artifactId><version>2.0.0</version>
          <packaging>pom</packaging>
          <modules><module>lib</module><module>app</module></modules>
          <properties><lib.artifact>lib</lib.artifact></properties>
        </project>"#,
    );
    write(
        &root,
        "lib/pom.xml",
        r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <parent><groupId>com.example</groupId><artifactId>root</artifactId><version>2.0.0</version></parent>
          <artifactId>lib</artifactId>
          <packaging>pom</packaging>
          <modules><module>core</module></modules>
        </project>"#,
    );
    write(
        &root,
        "lib/core/pom.xml",
        r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <parent><groupId>com.example</groupId><artifactId>lib</artifactId><version>2.0.0</version><relativePath>../pom.xml</relativePath></parent>
          <artifactId>core</artifactId>
        </project>"#,
    );
    write(
        &root,
        "app/pom.xml",
        r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <parent><groupId>com.example</groupId><artifactId>root</artifactId><version>2.0.0</version></parent>
          <artifactId>app</artifactId>
          <dependencies>
            <dependency><groupId>${project.groupId}</groupId><artifactId>${lib.artifact}</artifactId><version>${project.version}</version></dependency>
          </dependencies>
        </project>"#,
    );
    write(&root, "lib/core/src/main/java/Core.java", "class Core {}\n");
    write(
        &root,
        "lib/core/src/test/java/CoreTest.java",
        "class CoreTest { @Test void works() {} }\n",
    );
    let mut builder = GraphBuilder::new(&root, Revision::default()).unwrap();
    let report = builder.report().clone();
    let maven = report
        .adapter_outcomes
        .iter()
        .find(|outcome| outcome.adapter == faktor_impact::AdapterKind::Maven)
        .expect("maven outcome");
    assert!(
        maven.degraded,
        "the recursive pom model is the documented fallback: {maven:?}"
    );
    let graph = builder.build().unwrap();
    assert!(has_node(&graph, "package:root", NodeKind::Package));
    assert!(has_node(&graph, "package:lib", NodeKind::Package));
    assert!(has_node(&graph, "package:core", NodeKind::Package));
    assert!(has_node(&graph, "package:app", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:app",
        "package:lib",
        EdgeKind::DependsOn
    ));
    assert!(graph
        .files
        .iter()
        .any(
            |file| file.path.ends_with("lib/core/src/test/java/CoreTest.java") && file.test_target
        ));
}

#[test]
fn maven_modules_outside_the_workspace_are_refused() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    write(
        &root,
        "pom.xml",
        r#"<project><groupId>g</groupId><artifactId>root</artifactId><version>1</version>
        <modules><module>../outside</module><module>ok</module></modules></project>"#,
    );
    write(
        parent.path(),
        "outside/pom.xml",
        r#"<project><groupId>g</groupId><artifactId>outside</artifactId><version>1</version></project>"#,
    );
    write(
        &root,
        "ok/pom.xml",
        r#"<project><groupId>g</groupId><artifactId>ok</artifactId><version>1</version></project>"#,
    );
    let mut builder = GraphBuilder::new(&root, Revision::default()).unwrap();
    let graph = builder.build().unwrap();
    assert!(graph.nodes.iter().all(|node| node.name != "outside"));
    assert!(has_node(&graph, "package:ok", NodeKind::Package));
    let report = builder.report().clone();
    assert!(report
        .skipped
        .iter()
        .any(|skip| skip.contains("escapes the workspace root")));
}

#[test]
fn python_installed_metadata_resolves_dynamic_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pyproject.toml",
        r#"[project]
name = "dynamic-pkg"
version = "1.0.0"
dynamic = ["dependencies"]
"#,
    );
    write(
        root,
        "dynamic_pkg-1.0.0.dist-info/METADATA",
        "Metadata-Version: 2.1\nName: dynamic-pkg\nVersion: 1.0.0\nRequires-Dist: requests>=2\nRequires-Dist: pydantic; extra == \"dev\"\n\nbody\n",
    );
    write(
        root,
        "dynamic_pkg/module.py",
        "def add(a, b):\n    return a + b\n",
    );
    write(
        root,
        "tests/test_module.py",
        "from dynamic_pkg.module import add\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    );
    let mut builder = GraphBuilder::new(root, Revision::default()).unwrap();
    let report = builder.report().clone();
    let python = report
        .adapter_outcomes
        .iter()
        .find(|outcome| outcome.adapter == faktor_impact::AdapterKind::Python)
        .expect("python outcome");
    assert!(
        !python.degraded,
        "installed metadata must serve the dependency set: {python:?}"
    );
    let graph = builder.build().unwrap();
    assert!(has_node(&graph, "package:dynamic-pkg", NodeKind::Package));
    assert!(has_node(&graph, "package:requests", NodeKind::Package));
    assert!(has_node(&graph, "package:pydantic", NodeKind::Package));
    assert!(graph
        .edges
        .iter()
        .any(|edge| edge.from == "package:dynamic-pkg" && edge.kind == EdgeKind::DependsOn));
    let impacted = faktor_impact::ImpactGraph::new(graph)
        .unwrap()
        .affected(
            &[ChangedTarget::file("dynamic_pkg/module.py")],
            &AffectedOptions::default(),
        )
        .unwrap();
    assert!(impacted.tests.iter().any(|test| test.name == "test_add"));
}

#[test]
fn gradle_native_probe_is_supervised_bounded_and_degraded_when_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "settings.gradle",
        "rootProject.name = 'native'\ninclude ':lib', ':app'\n",
    );
    write(root, "lib/build.gradle", "plugins { id 'java' }\n");
    write(
        root,
        "app/build.gradle",
        "plugins { id 'java' }\ndependencies { implementation project(':lib') }\n",
    );
    write(root, "lib/src/main/java/Lib.java", "class Lib {}\n");
    write(root, "app/src/main/java/App.java", "class App {}\n");
    let cas_root = tempfile::tempdir().unwrap();
    let supervisor =
        ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(cas_root.path().join("cas"))));
    let options = TopologyOptions {
        supervisor: Some(Arc::clone(&supervisor)),
        ..TopologyOptions::default()
    };
    let mut builder = GraphBuilder::new_with_topology(root, Revision::default(), options).unwrap();
    let report = builder.report().clone();
    let gradle = report
        .adapter_outcomes
        .iter()
        .find(|outcome| outcome.adapter == faktor_impact::AdapterKind::Gradle)
        .expect("gradle outcome");
    let graph = builder.build().unwrap();
    assert!(has_node(&graph, "package:lib", NodeKind::Package));
    assert!(has_node(&graph, "package:app", NodeKind::Package));
    assert!(has_edge(
        &graph,
        "package:app",
        "package:lib",
        EdgeKind::DependsOn
    ));
    if gradle.mode == faktor_impact::AdapterMode::NativeMetadata {
        assert!(!gradle.degraded, "{gradle:?}");
    } else {
        assert!(gradle.degraded, "{gradle:?}");
        assert!(
            gradle.detail.contains("unavailable") || gradle.detail.contains("exited"),
            "{gradle:?}"
        );
    }
    // A supervised probe never leaves a child behind.
    let spawns: Vec<_> = supervisor
        .recent_spawns()
        .into_iter()
        .filter(|spawn| spawn.argv.starts_with("gradle "))
        .collect();
    for spawn in spawns {
        assert!(
            spawn.exited_ms.is_some(),
            "gradle probe must be reaped, not orphaned: {spawn:?}"
        );
        assert!(
            spawn.argv.contains("--no-daemon"),
            "gradle must not start a daemon: {spawn:?}"
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    supervisor.reap();
    assert_eq!(
        supervisor.registered(),
        0,
        "no probe child may outlive the build"
    );
}

#[cfg(unix)]
#[test]
fn hostile_gradle_metadata_paths_are_refused_without_an_orphan() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "settings.gradle", "include ':ok'\n");
    write(root, "ok/build.gradle", "plugins { id 'java' }\n");
    let script = dir.path().join("fake-gradle");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' 'FAKTOR_TOPOLOGY:[{\"name\":\"ok\",\"path\":\":ok\",\"dir\":\"../escape\",\"deps\":[]}]'\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let cas_root = tempfile::tempdir().unwrap();
    let supervisor =
        ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(cas_root.path().join("cas"))));
    let probe = faktor_impact::NativeProbe {
        supervisor: Some(Arc::clone(&supervisor)),
        timeout: std::time::Duration::from_secs(5),
        max_bytes: 1024 * 1024,
        enabled: true,
        gradle_command: Some(script.to_string_lossy().into_owned()),
    };
    let topology = faktor_impact::topology::discover_gradle_native(root, &probe);
    let outcome = topology.outcomes.first().expect("outcome");
    assert!(
        outcome.degraded,
        "hostile metadata must fall back: {outcome:?}"
    );
    assert!(outcome.detail.contains("escapes"), "{outcome:?}");
    assert!(
        topology
            .units
            .values()
            .all(|unit| !unit.dir.contains("escape")),
        "a hostile native directory must never become a unit: {topology:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    supervisor.reap();
    assert_eq!(supervisor.registered(), 0, "the fake probe must be reaped");
}
