plugins {
    kotlin("jvm") version "2.4.20" apply false
    id("org.jetbrains.intellij.platform") version "2.18.1" apply false
}

// The executable smoke mains of each module: they probe application behavior
// (real daemon, canned frames, parity matrix) and are not JUnit tests, so the
// Gradle `smoke` tasks run them on the test runtime classpath.
val smokeMains = mapOf(
    "backend" to listOf(
        "Backend" to "dev.faktor.backend.BackendSmoke",
        "NativeBridge" to "dev.faktor.backend.NativeBridgeSmoke"
    ),
    "frontend" to listOf(
        "Frontend" to "dev.faktor.frontend.FrontendSmoke",
        "ControlPlaneCredential" to "dev.faktor.frontend.ControlPlaneCredentialSmoke",
        "JetBrainsParity" to "dev.faktor.frontend.JetBrainsParitySmoke"
    )
)

fun faktorRepoRoot(): File {
    var dir: File? = rootDir.absoluteFile
    while (dir != null &&
        !(File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory)
    ) {
        dir = dir.parentFile
    }
    require(dir != null) {
        "Faktor repository root (Cargo.toml + crates/) not found above $rootDir"
    }
    return dir
}

val faktorRepo = faktorRepoRoot()

allprojects {
    group = "dev.faktor"
    version = "0.1.0"
    repositories {
        mavenCentral()
    }

    // Dependency locking: every resolvable configuration is locked; the
    // committed `gradle.lockfile` files are the reviewable dependency set.
    // Regenerate deliberately with `./gradlew --write-locks :frontend:buildPlugin`.
    dependencyLocking {
        lockAllConfigurations()
    }
}

subprojects {
    apply(plugin = "kotlin")

    extensions.configure<org.jetbrains.kotlin.gradle.dsl.KotlinJvmProjectExtension> {
        jvmToolchain(17)
        compilerOptions {
            jvmDefault.set(org.jetbrains.kotlin.gradle.dsl.JvmDefaultMode.NO_COMPATIBILITY)
        }
    }

    tasks.withType<Test>().configureEach {
        failOnNoDiscoveredTests = false
    }

    dependencies {
        "compileOnly"("org.jetbrains.kotlin:kotlin-stdlib:1.9.22")

        if (project.name == "backend") {
            "implementation"(project(":shared"))
            "testImplementation"(kotlin("test"))
        }
    }

    val mains = smokeMains[project.name].orEmpty()
    if (mains.isNotEmpty()) {
        val testRuntime =
            extensions.getByType<SourceSetContainer>().named("test").get().runtimeClasspath
        // kotlin.stdlib.default.dependency=false leaves the stdlib off the
        // runtime classpath; the locked compileClasspath supplies it.
        val stdlib = configurations.getByName("compileClasspath")
            .filter { it.name.startsWith("kotlin-stdlib") }
        val javaExe = extensions
            .getByType<org.gradle.jvm.toolchain.JavaToolchainService>()
            .launcherFor {
                languageVersion.set(
                    org.gradle.jvm.toolchain.JavaLanguageVersion.of(17)
                )
            }
            .get().executablePath.asFile.absolutePath
        val smokeTasks = mains.map { (label, main) ->
            tasks.register<Exec>("smoke$label") {
                group = "verification"
                description = "Run the $label executable smoke"
                dependsOn(tasks.named("testClasses"))
                val smokeArgs = mutableListOf(
                    "-Dfaktor.repo.root=${faktorRepo.absolutePath}",
                    "-cp", (testRuntime + stdlib).asPath, main
                )
                findProperty("faktorCliBin")?.let { smokeArgs.add(it.toString()) }
                if (findProperty("writeBaselines") != null) {
                    smokeArgs.add("-Dfaktor.parity.writeBaselines=true")
                }
                // The smoke mints detached fake-daemon processes on purpose:
                // redirecting the JVM's stdout/stderr to a file keeps them
                // from holding Gradle's capture pipe open after the JVM exits
                // (which would hang the build), and the log is replayed.
                val log = layout.buildDirectory.file("smoke/$label.log")
                doFirst { log.get().asFile.parentFile.mkdirs() }
                commandLine(
                    listOf(
                        "sh", "-c",
                        "java_bin=\"${'$'}1\"; log=\"${'$'}2\"; shift 2; " +
                            "\"${'$'}java_bin\" \"${'$'}@\" >\"${'$'}log\" 2>&1; " +
                            "rc=${'$'}?; cat \"${'$'}log\"; exit ${'$'}rc",
                        "sh", javaExe, log.get().asFile.absolutePath
                    ) + smokeArgs
                )
            }
        }
        tasks.register("smoke") {
            group = "verification"
            description = "Run every executable smoke of this module"
            dependsOn(smokeTasks)
        }
    }
}

tasks.register("smoke") {
    group = "verification"
    description = "Run every Faktor JetBrains executable smoke (backend + frontend)"
    dependsOn(":backend:smoke", ":frontend:smoke")
}
