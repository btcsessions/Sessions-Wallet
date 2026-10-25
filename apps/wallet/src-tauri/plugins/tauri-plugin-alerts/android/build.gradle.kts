import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.btcsessions.alerts"
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
    // NotificationCompat, ServiceCompat (foreground service types).
    implementation("androidx.core:core:1.13.1")
    // ActivityResult, for the settings screens the plugin opens.
    implementation("androidx.activity:activity:1.8.0")
    implementation(project(":tauri-android"))
}
