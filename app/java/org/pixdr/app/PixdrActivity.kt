package org.pixdr.app

import android.app.NativeActivity
import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log

class PixdrActivity : NativeActivity() {
    private external fun nativeOnFilePicked(
        requestCode: Int,
        uri: String,
        displayName: String,
        mimeType: String,
        fd: Int,
    )

    fun openFilePicker(requestCode: Int, mimeType: String) {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = mimeType
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
        }
        startActivityForResult(intent, requestCode)
    }

    fun chooseTxImage() {
        openFilePicker(REQUEST_TX_IMAGE, "image/*")
    }

    @Deprecated("NativeActivity bridge uses the legacy result callback intentionally")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != Activity.RESULT_OK) {
            return
        }
        val uri = data?.data ?: return
        val flags = data.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION
        try {
            contentResolver.takePersistableUriPermission(uri, flags)
        } catch (e: Exception) {
            Log.w(TAG, "Could not persist URI permission", e)
        }
        val fd = try {
            contentResolver.openFileDescriptor(uri, "r")?.detachFd() ?: -1
        } catch (e: Exception) {
            Log.w(TAG, "Could not open selected file", e)
            -1
        }
        nativeOnFilePicked(
            requestCode,
            uri.toString(),
            displayName(uri),
            contentResolver.getType(uri) ?: "",
            fd,
        )
    }

    private fun displayName(uri: Uri): String {
        var name = ""
        val cursor = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
        try {
            if (cursor != null && cursor.moveToFirst()) {
                val index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (index >= 0) {
                    name = cursor.getString(index) ?: ""
                }
            }
        } finally {
            cursor?.close()
        }
        return if (name.isNotBlank()) name else uri.lastPathSegment ?: "selected file"
    }

    companion object {
        private const val TAG = "pixdr"
        const val REQUEST_TX_IMAGE = 4201

        init {
            System.loadLibrary("pixdr")
        }
    }
}
