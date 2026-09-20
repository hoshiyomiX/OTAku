// Top-level build file for OTAku
// Configures plugins used across all subprojects
// Native backend: Rust cdylib (libotaku_native.so) built by cargo-ndk

buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        // T42: AGP 8.4.0 → 8.7.3 — official compileSdk 35 support (needed by
        // material 1.14.0's androidx.core 1.16.0). Gradle 8.9+ required
        // (wrapper + CI workflow updated in the same commit).
        classpath("com.android.tools.build:gradle:8.7.3")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:1.9.24")
    }
}

tasks.register("clean", Delete::class) {
    delete(rootProject.layout.buildDirectory)
}
