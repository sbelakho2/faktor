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
        "JetBrainsParity" to "dev.faktor.frontend.JetBrainsParitySmoke",
        "HostMatrix" to "dev.faktor.frontend.JetBrainsHostMatrixSmoke"
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

// Pinned core-fonts environment for the offscreen Swing render (see the file's
// header). Only applied on Linux hosts; macOS/Windows record their own
// environment fingerprints. The smoke JVM runs with a checkout-local
// `user.home` so the JVM's per-user fontconfig cache cannot keep a stale
// host-default mapping and produce a false environment mismatch.
val coreFontsConf =
    File(faktorRepo, "apps/jetbrains/frontend/src/test/resources/parity/fonts/core-fonts.conf")

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
                // JVM system properties MUST precede the main class; CLI args
                // follow it. Writing the re-pin flag after `main` made it a
                // program argument, so `-Dfaktor.parity.writeBaselines` was
                // never set and the re-pin silently compared instead of
                // pinning.
                val jvmArgs = mutableListOf("-Dfaktor.repo.root=${faktorRepo.absolutePath}")
                if (findProperty("writeBaselines") != null) {
                    jvmArgs.add("-Dfaktor.parity.writeBaselines=true")
                }
                val smokeHome = layout.buildDirectory.dir("smoke/home").get().asFile
                jvmArgs.add("-Duser.home=${smokeHome.absolutePath}")
                val smokeArgs = mutableListOf<String>()
                smokeArgs.addAll(jvmArgs)
                smokeArgs.add("-cp")
                smokeArgs.add((testRuntime + stdlib).asPath)
                smokeArgs.add(main)
                findProperty("faktorCliBin")?.let { smokeArgs.add(it.toString()) }
                // The smoke mints detached fake-daemon processes on purpose:
                // redirecting the JVM's stdout/stderr to a file keeps them
                // from holding Gradle's capture pipe open after the JVM exits
                // (which would hang the build), and the log is replayed.
                val log = layout.buildDirectory.file("smoke/$label.log")
                doFirst {
                    log.get().asFile.parentFile.mkdirs()
                    smokeHome.mkdirs()
                }
                if (coreFontsConf.isFile &&
                    System.getProperty("os.name").lowercase().contains("linux")
                ) {
                    environment("FONTCONFIG_FILE", coreFontsConf.absolutePath)
                }
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

        // The packaged-plugin host matrix (frontend only): builds the plugin
        // ZIP, extracts it, and runs the host matrix with the extracted
        // `faktor/lib/*.jar` FIRST on the classpath, so production classes
        // load from the SHIPPED artifact and the smoke asserts that
        // provenance. Separate from `:frontend:smoke` so the common smoke
        // stays network/toolchain-light; the trusted jetbrains-smoke lane
        // enforces it via FAKTOR_JETBRAINS_REQUIRE_PLUGIN_ZIP=1.
        if (project.name == "frontend") {
            val pluginZip = layout.buildDirectory.file("distributions/faktor-${version}.zip")
            val extractDir = layout.buildDirectory.dir("smoke/plugin-zip")
            val extractPluginZip = tasks.register<Sync>("extractPluginZip") {
                group = "verification"
                description = "Extract the built JetBrains plugin ZIP for the host matrix"
                dependsOn(tasks.named("buildPlugin"))
                from({ zipTree(pluginZip.get().asFile) })
                into(extractDir)
            }
            val matrixLog = layout.buildDirectory.file("smoke/HostMatrixZip.log")
            val matrixHome = layout.buildDirectory.dir("smoke/home").get().asFile
            tasks.register<Exec>("smokeHostMatrixZip") {
                group = "verification"
                description = "Run the JetBrains host matrix against the BUILT plugin ZIP"
                dependsOn(extractPluginZip, tasks.named("testClasses"))
                val libs = extractDir.get().dir("faktor/lib").asFile.absolutePath
                val matrixArgs = mutableListOf(
                    "-Dfaktor.repo.root=${faktorRepo.absolutePath}",
                    "-Duser.home=${matrixHome.absolutePath}",
                    "-Dfaktor.hostMatrix.requireZip=true",
                    "-Dfaktor.hostMatrix.zip=${pluginZip.get().asFile.absolutePath}",
                    "-cp",
                    "$libs/*${File.pathSeparator}${(testRuntime + stdlib).asPath}",
                    "dev.faktor.frontend.JetBrainsHostMatrixSmoke"
                )
                findProperty("faktorCliBin")?.let { matrixArgs.add(it.toString()) }
                doFirst {
                    matrixLog.get().asFile.parentFile.mkdirs()
                    matrixHome.mkdirs()
                }
                if (coreFontsConf.isFile &&
                    System.getProperty("os.name").lowercase().contains("linux")
                ) {
                    environment("FONTCONFIG_FILE", coreFontsConf.absolutePath)
                }
                commandLine(
                    listOf(
                        "sh", "-c",
                        "java_bin=\"${'$'}1\"; log=\"${'$'}2\"; shift 2; " +
                            "\"${'$'}java_bin\" \"${'$'}@\" >\"${'$'}log\" 2>&1; " +
                            "rc=${'$'}?; cat \"${'$'}log\"; exit ${'$'}rc",
                        "sh", javaExe, matrixLog.get().asFile.absolutePath
                    ) + matrixArgs
                )
            }
        }
    }
}

tasks.register("smoke") {
    group = "verification"
    description = "Run every Faktor JetBrains executable smoke (backend + frontend)"
    dependsOn(":backend:smoke", ":frontend:smoke")
}

tasks.register("smokeHostMatrixZip") {
    group = "verification"
    description = "Run the JetBrains host matrix proof against the BUILT plugin ZIP"
    dependsOn(":frontend:smokeHostMatrixZip")
}
