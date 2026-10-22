import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.btcsessions.passkey"
    compileSdk = 36

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

dependencies {
    // Credential Manager: passkeys with the WebAuthn PRF extension.
    implementation("androidx.credentials:credentials:1.6.0")
    // Passkeys from Google Password Manager on Android 9 to 13 (and as the
    // default provider after that).
    implementation("androidx.credentials:credentials-play-services-auth:1.6.0")
    implementation(project(":tauri-android"))
}
