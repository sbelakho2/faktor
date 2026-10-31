import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    id("org.jetbrains.intellij.platform")
}

// HAR-16 (two-IDE-version matrix): the IntelliJ Platform release this build
// resolves, selected by the documented project property `faktor.ideaVersion`
// (`-Pfaktor.ideaVersion=<release>`). With the property absent the build is
// byte-identical to before: the pinned stable release from
// `faktor.ideaVersion.pin` (gradle.properties, the single source of truth
// shared with the root build script). The documented alternate leg selects an
// OLDER stable release — 2024.1 is the oldest release line the plugin declares
// support for (pluginConfiguration ideaVersion.sinceBuild = "241"), so it is
// the honest lower bound of the real-IDE compatibility claim.
val faktorIdeaPin = (findProperty("faktor.ideaVersion.pin") as String?)
    ?.trim()
    ?.takeIf { it.isNotEmpty() }
    ?: error("gradle.properties must pin faktor.ideaVersion.pin (the default IntelliJ Platform release)")
val faktorIdeaVersionExplicit =
    (findProperty("faktor.ideaVersion") as String?)?.trim().orEmpty()
val faktorIdeaVersion = faktorIdeaVersionExplicit.ifEmpty { faktorIdeaPin }
val faktorIdeaAlternateRelease =
    faktorIdeaVersionExplicit.isNotEmpty() && faktorIdeaVersion != faktorIdeaPin

// HAR-16 locking policy: the committed gradle.lockfile pins
// idea:ideaIC:2024.1.7 and the IntelliJ platform test-framework modules track
// the IDE build number, so the alternate-release resolution is a DIFFERENT
// dependency set than the committed lock state. The default pin keeps full
// locking (no behavior change); an explicitly selected alternate release runs
// this project's configurations unlocked — integrity for the downloaded IDE
// artifacts comes from gradle/verification-metadata.xml, which the two-ide
// runner (apps/jetbrains/two-ide-matrix.sh) refreshes with
// `--write-verification-metadata sha256` for the leg and restores afterwards.
// backend/shared keep their locks either way; check-gradle-integrity.sh still
// enforces lockAllConfigurations() plus committed lockfiles for every project.
if (faktorIdeaAlternateRelease) {
    configurations.configureEach {
        resolutionStrategy.deactivateDependencyLocking()
    }
}

intellijPlatform {
    projectName = "faktor"
    buildSearchableOptions = false
    sandboxContainer = rootProject.layout.buildDirectory.dir("intellijPlatform/sandbox")

    pluginConfiguration {
        id = "dev.faktor.jetbrains"
        name = "Faktor"
        version = "0.1.0"
        description = "Faktor — an engineering agent for autonomous code investigation, implementation and verification inside your IDE."
        vendor {
            name = "Faktor"
        }
        ideaVersion {
            sinceBuild = "241"
            untilBuild = provider { null }
        }
    }

    pluginVerification {
        ides {
            current()
        }
    }
}

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

dependencies {
    implementation(project(":backend"))
    implementation(project(":shared"))

    intellijPlatform {
        // HAR-16: pinned release by default; `-Pfaktor.ideaVersion=<release>`
        // selects another (documented alternate: 2024.1, the oldest 241-line
        // stable release) — see the property block at the top of this file.
        intellijIdeaCommunity(faktorIdeaVersion)
        // The platform test framework: IdeJourneyTest runs the REAL IntelliJ
        // application (ProjectManager project + plugin.xml extensions + tool
        // window manager) in-process.
        testFramework(TestFrameworkType.Platform)
    }

    testImplementation("junit:junit:4.13.2")
}

/**
 * The real-IDE host journey lane: opens a platform project, shows the Faktor
 * tool window, focuses + types in the composer and dispatches the send action,
 * then writes `target/certification/jetbrains-ide-journey/` evidence.
 *
 * It reuses the plugin-configured `test` task (the IntelliJ Platform Gradle
 * plugin owns that task's platform JVM setup), narrowed to `IdeJourneyTest`
 * with the `faktor.ide.journey` gate set. A plain `:frontend:test` run skips
 * the journey (it prints `IDE JOURNEY SKIPPED`), so the platform journey is
 * only ever claimed by this task or by `apps/jetbrains/ide-journey.sh`, which
 * first probes that an IDE distribution is cached or reachable and records a
 * typed skip artifact when this host cannot execute it.
 */
val ideJourney = tasks.register("ideJourney") {
    group = "verification"
    description =
        "Run the real IntelliJ Platform host journey inside the platform test " +
            "application (requires the cached or downloadable IntelliJ IDEA " +
            "Community $faktorIdeaVersion distribution)."
    dependsOn(tasks.named("test"))
}

gradle.taskGraph.whenReady {
    if (allTasks.any { it.name == "ideJourney" }) {
        tasks.named<Test>("test") {
            filter {
                includeTestsMatching("dev.faktor.frontend.IdeJourneyTest")
                isFailOnNoMatchingTests = false
            }
            systemProperty("faktor.ide.journey", "1")
            // The journey must not touch the developer's ~/.faktor home.
            environment(
                "FAKTOR_DATA_DIR",
                layout.buildDirectory.dir("ide-journey/data").get().asFile.absolutePath
            )
        }
    }
}
