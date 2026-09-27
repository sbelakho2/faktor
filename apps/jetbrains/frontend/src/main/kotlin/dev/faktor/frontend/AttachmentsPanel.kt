// File attachments for the Task entry: a file chooser plus a drop list
// (java.awt.dnd via Swing's TransferHandler) whose paths are submitted as
// the `files` array of the native task-run request. Pure presentation: the
// panel only owns the path list; callers read [files] when starting a run.
//
// Image parity: allowlisted image files picked here are read (bounded) by
// the caller and uploaded to the daemon as durable binary attachments, so
// the task start carries the SAME typed artifact ids as the VS Code
// client ([AttachmentImages]). Documented platform gap: the chooser/DnD
// surface yields filesystem paths only, so an unsaved clipboard image has
// no path and cannot be attached yet (a saved screenshot works).
package dev.faktor.frontend

import dev.faktor.shared.NativeAttachmentLimits
import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.asciiLowerCase
import java.awt.BorderLayout
import java.awt.FlowLayout
import java.awt.datatransfer.DataFlavor
import java.io.File
import java.nio.file.Paths
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JFileChooser
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.ListSelectionModel
import javax.swing.TransferHandler

/**
 * The image half of the attachment contract (parity with the VS Code
 * client): the closed daemon mime allowlist, the daemon-wide per-image
 * byte bound, and a bounded read. An allowlisted image becomes a durable
 * binary attachment (uploaded before the task start); everything else
 * stays a workspace path in `files`.
 */
object AttachmentImages {

    /** Emergency fallback per-image bound (daemon-wide default). */
    const val MAX_IMAGE_BYTES = 5L * 1024 * 1024

    /** Emergency fallback per-document bound (daemon-wide default). */
    const val MAX_DOCUMENT_BYTES = 8L * 1024 * 1024

    /** Emergency fallback decoded-byte upload ceiling. */
    const val MAX_UPLOAD_BYTES = 7L * 1024 * 1024

    /** Emergency image allowlist (daemon-wide defaults). */
    val EMERGENCY_IMAGE_MIMES = listOf("image/png", "image/jpeg", "image/gif", "image/webp")

    /** Emergency document allowlist (daemon-wide defaults). */
    val EMERGENCY_DOCUMENT_MIMES = listOf("application/pdf", "text/plain")

    /**
     * The attachment admission policy of ONE chosen model: the daemon's
     * advertised `/models` numbers (`source = "advertised"`) or the
     * conservative emergency ceiling when the catalog is unavailable. The
     * client never mirrors the daemon's live numbers.
     */
    data class Policy(
        val source: String,
        val maxUploadBytes: Long,
        val maxAttachmentBytes: Long,
        val imageMimes: List<String>,
        val maxImageBytes: Long,
        val maxRequestImageBytes: Long,
        val documentCapable: Boolean,
        val documentMimes: List<String>,
        val maxDocumentBytes: Long,
        val maxRequestDocumentBytes: Long
    )

    fun emergencyPolicy(): Policy = Policy(
        source = "emergency",
        maxUploadBytes = MAX_UPLOAD_BYTES,
        maxAttachmentBytes = 32L * 1024 * 1024,
        imageMimes = EMERGENCY_IMAGE_MIMES,
        maxImageBytes = MAX_IMAGE_BYTES,
        maxRequestImageBytes = 16L * 1024 * 1024,
        // Unknown until advertised: a legacy daemon decides document
        // admission itself, so the emergency policy never refuses on
        // capability grounds.
        documentCapable = false,
        documentMimes = EMERGENCY_DOCUMENT_MIMES,
        maxDocumentBytes = MAX_DOCUMENT_BYTES,
        maxRequestDocumentBytes = 16L * 1024 * 1024
    )

    /** Turn the daemon's advertised limits into the client policy. */
    fun policyFromLimits(limits: NativeAttachmentLimits): Policy = Policy(
        source = "advertised",
        maxUploadBytes = limits.maxUploadBytes,
        maxAttachmentBytes = limits.maxAttachmentBytes,
        imageMimes = limits.image.mimes.map { it.mime },
        maxImageBytes = limits.image.mimes.map { it.maxBytes }.minOrNull() ?: MAX_IMAGE_BYTES,
        maxRequestImageBytes = limits.image.maxRequestBytes,
        documentCapable = limits.document.capable,
        documentMimes = limits.document.mimes.map { it.mime },
        maxDocumentBytes = limits.document.mimes.map { it.maxBytes }.minOrNull() ?: MAX_DOCUMENT_BYTES,
        maxRequestDocumentBytes = limits.document.maxRequestBytes
    )

    /** The policy for one provider/model from the fetched catalog. */
    fun policyForModel(catalog: List<NativeModelInfo>, provider: String, model: String): Policy {
        val entry = catalog.firstOrNull { it.provider == provider && it.model == model }
            ?: return emergencyPolicy()
        val limits = entry.attachmentLimits ?: return emergencyPolicy()
        return policyFromLimits(limits)
    }

    /** The daemon-deliverable mime for `path`, or null (stays a path). */
    fun mimeOf(path: String): String? = when (asciiLowerCase(path.substringAfterLast('.', ""))) {
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        else -> null
    }

    /** The deliverable DOCUMENT mime for `path`, or null (stays a path). */
    fun documentMimeOf(path: String): String? =
        when (asciiLowerCase(path.substringAfterLast('.', ""))) {
            "pdf" -> "application/pdf"
            "txt" -> "text/plain"
            else -> null
        }

    /**
     * Bounded read of one image file: null when `file` is not a regular
     * file or exceeds [MAX_IMAGE_BYTES] (never a truncated or unbounded
     * read). The bound is enforced DURING the read, so a growing file can
     * still never exceed it.
     */
    fun readBounded(file: File, max: Long = MAX_IMAGE_BYTES): ByteArray? {
        if (!file.isFile || file.length() > max) return null
        val out = java.io.ByteArrayOutputStream()
        val buffer = ByteArray(8192)
        file.inputStream().use { input ->
            while (true) {
                val read = input.read(buffer)
                if (read < 0) break
                if (out.size().toLong() + read > max) return null
                out.write(buffer, 0, read)
            }
        }
        return out.toByteArray()
    }
}

class AttachmentsPanel : JPanel(BorderLayout()) {

    private val paths = DefaultListModel<String>()

    private val list = JList(paths)

    private val addButton = JButton("Add files...")

    private val removeButton = JButton("Remove")

    private val clearButton = JButton("Clear")

    init {
        list.selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
        list.toolTipText = "drop files here to attach them to the task"
        list.transferHandler = object : TransferHandler() {
            override fun canImport(support: TransferSupport): Boolean =
                support.isDataFlavorSupported(DataFlavor.javaFileListFlavor)

            override fun importData(support: TransferSupport): Boolean {
                if (!canImport(support)) return false
                @Suppress("UNCHECKED_CAST")
                val files = support.transferable
                    .getTransferData(DataFlavor.javaFileListFlavor) as List<File>
                addFiles(files.map { it.absolutePath })
                return true
            }
        }

        val buttons = JPanel(FlowLayout(FlowLayout.LEFT, 4, 0))
        buttons.add(addButton)
        buttons.add(removeButton)
        buttons.add(clearButton)

        addButton.addActionListener { chooseFiles() }
        removeButton.addActionListener {
            val selected = list.selectedValuesList
            for (path in selected) paths.removeElement(path)
        }
        clearButton.addActionListener { paths.clear() }

        val body = JPanel(BorderLayout(0, 2))
        body.add(JScrollPane(list), BorderLayout.CENTER)
        body.add(buttons, BorderLayout.SOUTH)
        add(body, BorderLayout.CENTER)
    }

    /** The attached absolute paths, in list order (the task-run `files` array). */
    fun files(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until paths.size()) out.add(paths.getElementAt(i))
        return out
    }

    fun addFiles(pathsToAdd: List<String>) {
        for (path in pathsToAdd) {
            val normalized = normalize(path) ?: continue
            if (!contains(normalized)) paths.addElement(normalized)
        }
    }

    fun count(): Int = paths.size()

    fun clear() {
        paths.clear()
    }

    private fun chooseFiles() {
        val chooser = JFileChooser()
        chooser.isMultiSelectionEnabled = true
        chooser.fileSelectionMode = JFileChooser.FILES_ONLY
        if (chooser.showOpenDialog(this) == JFileChooser.APPROVE_OPTION) {
            val selected = chooser.selectedFiles ?: emptyArray()
            addFiles(selected.map { it.absolutePath })
        }
    }

    private fun normalize(path: String): String? {
        val trimmed = path.trim()
        if (trimmed.isEmpty()) return null
        return try {
            Paths.get(trimmed).toAbsolutePath().normalize().toString()
        } catch (e: Exception) {
            null
        }
    }

    private fun contains(path: String): Boolean {
        for (i in 0 until paths.size()) {
            if (paths.getElementAt(i) == path) return true
        }
        return false
    }
}
