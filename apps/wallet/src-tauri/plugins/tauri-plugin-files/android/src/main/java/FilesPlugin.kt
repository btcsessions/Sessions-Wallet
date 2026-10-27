package io.btcsessions.files

import android.app.Activity
import android.content.Intent
import android.util.Log
import androidx.activity.result.ActivityResult
import androidx.core.content.FileProvider
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File

private const val TAG = "SessionsFiles"

@InvokeArg
class FileArgs {
    var filename: String = "export.txt"
    var mime: String = "text/plain"
    var content: String = ""
}

/** Wallet exports: share a file to another app, or save it where the user picks. */
@TauriPlugin
class FilesPlugin(private val activity: Activity) : Plugin(activity) {
    /** Content waiting for the save-as screen to come back. */
    private var pending: String? = null

    private fun safeName(name: String) = name.replace(Regex("[^A-Za-z0-9._ -]"), "_").take(80).ifBlank { "export.txt" }

    @Command
    fun share(invoke: Invoke) {
        val args = invoke.parseArgs(FileArgs::class.java)
        try {
            val dir = File(activity.cacheDir, "exports").apply { mkdirs() }
            // One export at a time: nothing lingers in the cache.
            dir.listFiles()?.forEach { it.delete() }
            val file = File(dir, safeName(args.filename))
            file.writeText(args.content)
            val uri = FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", file)
            val send = Intent(Intent.ACTION_SEND).apply {
                type = args.mime
                putExtra(Intent.EXTRA_STREAM, uri)
                putExtra(Intent.EXTRA_SUBJECT, file.name)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            activity.startActivity(Intent.createChooser(send, file.name))
            invoke.resolve(JSObject().put("done", true))
        } catch (e: Exception) {
            Log.w(TAG, "share failed", e)
            invoke.reject("Couldn't share the file: ${e.message}")
        }
    }

    @Command
    fun save(invoke: Invoke) {
        val args = invoke.parseArgs(FileArgs::class.java)
        pending = args.content
        val create = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = args.mime
            putExtra(Intent.EXTRA_TITLE, safeName(args.filename))
        }
        try {
            startActivityForResult(invoke, create, "saved")
        } catch (e: Exception) {
            pending = null
            invoke.reject("Couldn't open the save screen: ${e.message}")
        }
    }

    @ActivityCallback
    private fun saved(invoke: Invoke, result: ActivityResult) {
        val content = pending
        pending = null
        val uri = result.data?.data
        if (result.resultCode != Activity.RESULT_OK || uri == null || content == null) {
            invoke.resolve(JSObject().put("done", false))
            return
        }
        try {
            activity.contentResolver.openOutputStream(uri, "wt")?.use { it.write(content.toByteArray()) }
                ?: throw IllegalStateException("no output stream")
            invoke.resolve(JSObject().put("done", true))
        } catch (e: Exception) {
            Log.w(TAG, "save failed", e)
            invoke.reject("Couldn't save the file: ${e.message}")
        }
    }
}
