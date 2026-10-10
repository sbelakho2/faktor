import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    id("org.jetbrains.intellij.platform")
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
        intellijIdeaCommunity("2024.1.7")
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
            "Community 2024.1.7 distribution)."
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
