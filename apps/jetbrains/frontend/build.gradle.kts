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
    }
}
