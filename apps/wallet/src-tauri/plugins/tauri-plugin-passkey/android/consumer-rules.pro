# Tauri's own consumer rules keep @TauriPlugin classes and their @Command
# methods. Credential Manager finds its Play services provider by reflection:
# keep it in minified release builds.
-if class androidx.credentials.CredentialManager
-keep class androidx.credentials.playservices.** {
  *;
}
