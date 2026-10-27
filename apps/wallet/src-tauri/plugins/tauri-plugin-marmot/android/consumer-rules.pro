# Tauri keeps @TauriPlugin classes and their @Command methods.
# UniFFI / JNA: keep the generated bindings and JNA (reflection, native callbacks).
-keep class dev.ipf.marmotkit.** { *; }
-keep class io.crates.keyring.** { *; }
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-dontwarn java.awt.**
