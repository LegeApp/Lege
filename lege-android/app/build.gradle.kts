import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.legeapp.lege"
    compileSdk = 35
    ndkVersion = "26.1.10909125"

    defaultConfig {
        applicationId = "com.legeapp.lege"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main").java.srcDir("../java")
        getByName("main").jniLibs.srcDir("src/main/jniLibs")
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

/**
 * Builds the JNI cdylib into the ABI directory that Android Gradle Plugin
 * packages. `cargo-ndk` gets the exact NDK selected above from AGP rather than
 * relying on a machine-specific environment variable.
 */
val buildRustAndroid by tasks.registering(Exec::class) {
    val workspaceRoot = project.projectDir.parentFile.parentFile
    val jniLibs = layout.projectDirectory.dir("src/main/jniLibs")

    workingDir = workspaceRoot
    inputs.dir(workspaceRoot.resolve("lege-android/src"))
    inputs.dir(workspaceRoot.resolve("lege-process"))
    inputs.dir(workspaceRoot.resolve("lege-gpu"))
    inputs.dir(workspaceRoot.resolve("lege-ocr"))
    inputs.file(workspaceRoot.resolve("Cargo.toml"))
    inputs.file(workspaceRoot.resolve("Cargo.lock"))
    outputs.dir(jniLibs)

    environment("ANDROID_NDK_HOME", android.ndkDirectory.absolutePath)
    commandLine(
        "cargo", "ndk", "-t", "arm64-v8a", "--platform", "26",
        "-o", jniLibs.asFile.absolutePath,
        "build", "--package", "lege-android", "--profile", "android",
    )
}

tasks.named("preBuild").configure {
    dependsOn(buildRustAndroid)
}
