// File attachments for the Task entry: a file chooser plus a drop list
// (java.awt.dnd via Swing's TransferHandler) whose paths are submitted as
// the `files` array of the native task-run request. Picked/dropped paths
// are relativized against the SESSION WORKSPACE ROOT the panel was built
// with (the IntelliJ host passes `Project.basePath`, the standalone
// launcher its working directory; the same root rides session creation, so
// the daemon resolves the relative paths against it — its projection
// exposes no workspace path). An entry outside the root is refused TYPED
// and stays visible in the refusal list, never silently dropped and never
// sent as an absolute path (the daemon refuses absolute attached files).
// Pure presentation otherwise: callers read [files] when starting a run.
//
// Binary parity with the VS Code client ([AttachmentImages]): allowlisted
// image files and deliverable documents (PDF/plain text) picked here are
// read (bounded) by the caller and uploaded to the daemon as durable
// binary attachments, so the task start carries the SAME typed artifact
// ids; everything else stays a workspace path. A clipboard image (paste /
// image-flavor drop) has no filesystem path: it is converted to bounded
// PNG bytes IN MEMORY and staged as a separate pending binary attachment
// (never written to disk), then uploaded like any other image.
package dev.faktor.frontend

import dev.faktor.shared.NativeAttachmentId
import dev.faktor.shared.NativeAttachmentLimits
import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.ProtocolAttachmentRef
import dev.faktor.shared.asciiLowerCase
import java.awt.BorderLayout
import java.awt.Dimension
import java.awt.FlowLayout
import java.awt.GraphicsEnvironment
import java.awt.Image
import java.awt.Toolkit
import java.awt.datatransfer.Clipboard
import java.awt.datatransfer.DataFlavor
import java.awt.event.ActionEvent
import java.awt.event.InputEvent
import java.awt.event.KeyEvent
import java.awt.image.BufferedImage
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.OutputStream
import java.nio.file.Path
import java.nio.file.Paths
import java.util.Base64
import javax.imageio.ImageIO
import javax.swing.AbstractAction
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JFileChooser
import javax.swing.JComponent
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.KeyStroke
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
        maxImageBytes = limits.image.mimes.map { it.maxBytes }.min() ?: MAX_IMAGE_BYTES,
        maxRequestImageBytes = limits.image.maxRequestBytes,
        documentCapable = limits.document.capable,
        documentMimes = limits.document.mimes.map { it.mime },
        maxDocumentBytes = limits.document.mimes.map { it.maxBytes }.min() ?: MAX_DOCUMENT_BYTES,
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

    /** Pixel-count bound of one clipboard image (64 MiB at 4 bytes/pixel). */
    const val MAX_CLIPBOARD_PIXELS = 16_777_216L

    /**
     * Convert an in-memory [Image] (clipboard paste / image-flavor drop) to
     * bounded PNG bytes WITHOUT touching the filesystem: the encoder writes
     * into a bounded in-memory stream and the whole conversion is refused
     * (null) above the encoded byte bound or the pixel bound, so a hostile
     * clipboard can never grow host memory unboundedly.
     */
    fun pngBytes(image: Image, max: Long = MAX_IMAGE_BYTES): ByteArray? {
        val width = image.getWidth(null)
        val height = image.getHeight(null)
        if (width <= 0 || height <= 0) return null
        if (width.toLong() * height.toLong() > MAX_CLIPBOARD_PIXELS) return null
        val buffered = if (image is BufferedImage && image.type != BufferedImage.TYPE_CUSTOM) {
            image
        } else {
            val copy = BufferedImage(width, height, BufferedImage.TYPE_INT_ARGB)
            val graphics = copy.createGraphics()
            try {
                graphics.drawImage(image, 0, 0, null)
            } finally {
                graphics.dispose()
            }
            copy
        }
        val out = BoundedByteArrayOutputStream(max)
        return try {
            if (!ImageIO.write(buffered, "png", out)) null else out.toByteArray()
        } catch (e: BoundedOverflow) {
            null
        } catch (e: Exception) {
            null
        }
    }

    /** The bounded in-memory sink of the PNG encoder (never unbounded RAM). */
    private class BoundedByteArrayOutputStream(private val max: Long) : OutputStream() {
        private val delegate = ByteArrayOutputStream()

        override fun write(b: Int) {
            if (delegate.size().toLong() + 1 > max) throw BoundedOverflow()
            delegate.write(b)
        }

        override fun write(b: ByteArray, off: Int, len: Int) {
            if (delegate.size().toLong() + len > max) throw BoundedOverflow()
            delegate.write(b, off, len)
        }

        fun toByteArray(): ByteArray = delegate.toByteArray()
    }

    private class BoundedOverflow : java.io.IOException("bounded PNG write exceeded")
}

/**
 * One pending attachment held as BYTES (never a path): a clipboard image
 * converted in memory, staged separately from the workspace path list and
 * uploaded as a durable binary attachment at task start.
 */
class PendingBinaryAttachment(
    val mime: String,
    val filename: String?,
    val bytes: ByteArray
) {
    fun size(): Int = bytes.size

    fun base64(): String = Base64.getEncoder().encodeToString(bytes)

    override fun toString(): String =
        (filename ?: "clipboard") + " (" + mime + ", " + bytes.size + " bytes, in memory)"
}

/**
 * One validated, base64-ready upload of a task start. `key` is the retry
 * identity ([pathUploadKey]): kind + mime + filename + size + byte digest,
 * so a rename/re-select of identical bytes never reuses a stale durable id
 * while an unchanged reference does.
 */
class PlannedAttachmentUpload(
    val key: String,
    val mime: String,
    val filename: String?,
    val base64: String
)

/** The validated attachment plan of one task start. */
class AttachmentPlan(
    /** Workspace-relative paths (`files`), never uploaded and never absolute. */
    val pathFiles: List<String>,
    /** Durable binary uploads (images + deliverable documents), in entry order. */
    val uploads: List<PlannedAttachmentUpload>
)

/**
 * The TYPED client refusal of one attachment that the advertised model
 * contract can never deliver (unsupported MIME, an unadvertised document
 * capability, or an oversize bound). The code vocabulary mirrors the VS
 * Code client's admission refusals.
 */
class AttachmentRefusal(
    val code: String,
    message: String
) : IllegalStateException(message) {
    override fun toString(): String = message ?: code
}

/** The composer-side path bound (parity with the VS Code client's cap). */
const val MAX_ATTACHMENT_PATH_CHARS = 4096

/** The normalized absolute session workspace root, or null when none is known. */
private fun normalizedWorkspaceRoot(workspaceRoot: Path?): Path? =
    workspaceRoot?.toAbsolutePath()?.normalize()

/** The `/`-separated wire form of one workspace-relative path. */
private fun relativeWireForm(path: Path): String =
    path.joinToString("/") { it.toString() }

/**
 * The workspace-relative wire path of one attachment entry (a picked/dropped
 * absolute path or an already-relative composer path), or a typed
 * [AttachmentRefusal] when the entry is not a bounded, hostile-safe path
 * inside the session workspace. An absolute entry outside [workspaceRoot]
 * (or any entry when no root is known) is refused typed — never silently
 * dropped and never forwarded as an absolute path; `..` traversal and
 * control characters are refused with the VS Code client's vocabulary.
 */
fun workspaceRelativePath(path: String, workspaceRoot: Path?): String {
    val trimmed = path.trim()
    if (trimmed.isEmpty()) {
        throw AttachmentRefusal("invalid_path", "an attachment path is empty or whitespace-only")
    }
    if (trimmed.length > MAX_ATTACHMENT_PATH_CHARS) {
        throw AttachmentRefusal(
            "invalid_path",
            "attachment path exceeds $MAX_ATTACHMENT_PATH_CHARS characters"
        )
    }
    if (trimmed.any { it.isISOControl() }) {
        throw AttachmentRefusal("invalid_path", "attachment path \"$trimmed\" carries control characters")
    }
    val parsed = try {
        Paths.get(trimmed)
    } catch (e: Exception) {
        throw AttachmentRefusal("invalid_path", "attachment path \"$trimmed\" is not a valid path")
    }
    // Platform-independent absolute forms: a Unix root separator, a Windows
    // drive prefix or UNC backslash, or the platform's own absolute path.
    val windowsDrive = trimmed.length >= 2 && trimmed[0].isLetter() && trimmed[1] == ':' &&
        (trimmed.length == 2 || trimmed[2] == '/' || trimmed[2] == '\\')
    val rootRelative = trimmed.startsWith("/") || trimmed.startsWith("\\")
    if (parsed.isAbsolute || windowsDrive || rootRelative) {
        val root = normalizedWorkspaceRoot(workspaceRoot)
            ?: throw AttachmentRefusal(
                "workspace_unknown",
                "attachment \"$trimmed\" is absolute and no session workspace root is known to relativize it"
            )
        if (!parsed.isAbsolute) {
            throw AttachmentRefusal(
                "outside_workspace",
                "attachment \"$trimmed\" is outside the session workspace $root"
            )
        }
        val absolute = parsed.normalize()
        if (absolute == root || !absolute.startsWith(root)) {
            throw AttachmentRefusal(
                "outside_workspace",
                "attachment \"$trimmed\" is outside the session workspace $root"
            )
        }
        return relativeWireForm(root.relativize(absolute))
    }
    if (parsed.any { it.toString() == ".." }) {
        throw AttachmentRefusal(
            "outside_workspace",
            "attachment \"$trimmed\" traverses outside the workspace"
        )
    }
    val relative = relativeWireForm(parsed.normalize())
    if (relative.isEmpty()) {
        throw AttachmentRefusal("invalid_path", "attachment path \"$trimmed\" names no file")
    }
    return relative
}

/**
 * Read one attachment under the session workspace with AUTHORITATIVE object
 * identity (P1-JETBRAINS): the lexical check alone can be satisfied by an
 * in-workspace symlink that resolves outside, so the candidate's real path
 * (symlinks resolved) must be contained in the workspace's real path, and
 * the identity is RE-VALIDATED after the read — a check→read swap that
 * changed the opened object refuses instead of uploading outside content.
 * A missing/non-regular/oversized file is `null` (typed refusal upstream).
 */
internal fun readAttachmentBounded(path: String, workspaceRoot: Path?, maxBytes: Long): ByteArray? {
    val relative = workspaceRelativePath(path, workspaceRoot)
    val root = normalizedWorkspaceRoot(workspaceRoot)
        ?: throw AttachmentRefusal(
            "workspace_unknown",
            "attachment \"$path\" cannot be resolved without a session workspace root"
        )
    val rootReal = try {
        root.toRealPath()
    } catch (e: Exception) {
        throw AttachmentRefusal(
            "workspace_unknown",
            "session workspace $root cannot be resolved: ${e.message}"
        )
    }
    val candidate = root.resolve(relative)
    val real = try {
        candidate.toRealPath()
    } catch (e: NoSuchFileException) {
        return null
    } catch (e: Exception) {
        throw AttachmentRefusal("outside_workspace", "attachment \"$path\" cannot be resolved: ${e.message}")
    }
    if (real == rootReal || !real.startsWith(rootReal)) {
        throw AttachmentRefusal(
            "outside_workspace",
            "attachment \"$path\" resolves outside the session workspace $rootReal"
        )
    }
    val bytes = AttachmentImages.readBounded(real.toFile(), maxBytes) ?: return null
    val after = try {
        candidate.toRealPath()
    } catch (e: Exception) {
        throw AttachmentRefusal(
            "outside_workspace",
            "attachment \"$path\" changed identity during the read"
        )
    }
    if (after != real) {
        throw AttachmentRefusal(
            "outside_workspace",
            "attachment \"$path\" changed identity during the read"
        )
    }
    return bytes
}

/**
 * Validate every composer attachment against the daemon-advertised policy
 * and build the exact start plan: pending in-memory binaries first, then
 * allowlisted image paths, then deliverable document paths (application/pdf,
 * text/plain when the advertised model supports them), and finally every
 * other path untouched on the workspace-relative `files` vocabulary (no
 * blind uploads of workspace source files). Every entry is resolved against
 * [workspaceRoot]: an absolute path inside the root is relativized, an
 * entry outside it (or any entry without a known root) is a typed
 * [AttachmentRefusal] BEFORE any upload, so nothing partial ever reaches
 * the durable store and no absolute path ever rides the request. A legacy
 * (emergency) policy leaves document admission to the daemon, exactly like
 * the VS Code client.
 */
fun planAttachments(
    files: List<String>,
    binaries: List<PendingBinaryAttachment>,
    policy: AttachmentImages.Policy,
    workspaceRoot: Path? = null
): AttachmentPlan {
    val pathFiles = ArrayList<String>()
    val uploads = ArrayList<PlannedAttachmentUpload>()
    var imageBytesTotal = 0L
    var documentBytesTotal = 0L

    fun gateUpload(name: String, size: Long) {
        if (size > policy.maxUploadBytes) {
            throw AttachmentRefusal(
                "oversized_upload",
                "attachment $name of $size bytes exceeds the ${policy.maxUploadBytes} byte upload bound"
            )
        }
    }

    for (binary in binaries) {
        val name = binary.filename ?: "clipboard.png"
        if (!policy.imageMimes.contains(binary.mime)) {
            throw AttachmentRefusal(
                "unsupported_image_type",
                "image attachment $name has unsupported mime ${binary.mime}; " +
                    "deliverable types: ${policy.imageMimes.joinToString(", ")}"
            )
        }
        val size = binary.size().toLong()
        if (size > policy.maxImageBytes) {
            throw AttachmentRefusal(
                "oversized_image",
                "image attachment $name of $size bytes exceeds the " +
                    "${policy.maxImageBytes} byte per-image bound"
            )
        }
        gateUpload(name, size)
        imageBytesTotal += size
        if (imageBytesTotal > policy.maxRequestImageBytes) {
            throw AttachmentRefusal(
                "oversized_image",
                "image attachments total $imageBytesTotal bytes, over the " +
                    "${policy.maxRequestImageBytes} byte request image bound"
            )
        }
        uploads.add(
            PlannedAttachmentUpload(
                key = pathUploadKey("binary", binary.mime, binary.filename, binary.bytes),
                mime = binary.mime,
                filename = binary.filename,
                base64 = binary.base64()
            )
        )
    }

    for (path in files) {
        val imageMime = AttachmentImages.mimeOf(path)
        if (imageMime != null) {
            val name = File(path).name
            if (!policy.imageMimes.contains(imageMime)) {
                throw AttachmentRefusal(
                    "unsupported_image_type",
                    "image attachment $name has mime $imageMime which the selected model " +
                        "does not advertise as deliverable (deliverable types: " +
                        policy.imageMimes.joinToString(", ") + ")"
                )
            }
            val bytes = readAttachmentBounded(path, workspaceRoot, policy.maxImageBytes)
                ?: throw AttachmentRefusal(
                    "oversized_image",
                    "image attachment $name is not a regular file or exceeds the advertised " +
                        policy.maxImageBytes + " byte per-image bound"
                )
            gateUpload(name, bytes.size.toLong())
            imageBytesTotal += bytes.size
            if (imageBytesTotal > policy.maxRequestImageBytes) {
                throw AttachmentRefusal(
                    "oversized_image",
                    "image attachments total $imageBytesTotal bytes, over the " +
                        policy.maxRequestImageBytes + " byte request image bound"
                )
            }
            uploads.add(
                PlannedAttachmentUpload(
                    key = pathUploadKey("image", imageMime, name, bytes),
                    mime = imageMime,
                    filename = name,
                    base64 = Base64.getEncoder().encodeToString(bytes)
                )
            )
            continue
        }
        val documentMime = AttachmentImages.documentMimeOf(path)
        if (documentMime != null) {
            val name = File(path).name
            if (policy.source == "advertised") {
                if (!policy.documentCapable) {
                    throw AttachmentRefusal(
                        "unsupported_document_type",
                        "document attachment $name ($documentMime) cannot be delivered: the " +
                            "selected model does not advertise document input; remove it or " +
                            "select a document-capable model"
                    )
                }
                if (!policy.documentMimes.contains(documentMime)) {
                    throw AttachmentRefusal(
                        "unsupported_document_type",
                        "document attachment $name has unsupported mime $documentMime; " +
                            "deliverable document types: " + policy.documentMimes.joinToString(", ")
                    )
                }
            }
            val bytes = readAttachmentBounded(path, workspaceRoot, policy.maxDocumentBytes)
                ?: throw AttachmentRefusal(
                    "oversized_document",
                    "document attachment $name is not a regular file or exceeds the advertised " +
                        policy.maxDocumentBytes + " byte per-document bound"
                )
            gateUpload(name, bytes.size.toLong())
            documentBytesTotal += bytes.size
            if (documentBytesTotal > policy.maxRequestDocumentBytes) {
                throw AttachmentRefusal(
                    "oversized_document",
                    "document attachments total $documentBytesTotal bytes, over the " +
                        policy.maxRequestDocumentBytes + " byte request document bound"
                )
            }
            uploads.add(
                PlannedAttachmentUpload(
                    key = pathUploadKey("document", documentMime, name, bytes),
                    mime = documentMime,
                    filename = name,
                    base64 = Base64.getEncoder().encodeToString(bytes)
                )
            )
            continue
        }
        // Ordinary workspace source files keep the workspace-relative path
        // vocabulary: the run reads them from the session workspace, never
        // re-uploads, and an entry outside the workspace is refused typed.
        pathFiles.add(workspaceRelativePath(path, workspaceRoot))
    }
    return AttachmentPlan(pathFiles, uploads)
}

/**
 * The LOCAL retry-reuse key of one upload: kind + mime + FILENAME + declared
 * size + SHA-256 of the EXACT bytes. This is a client-local reuse key, NOT
 * the daemon's CAS/attachment identity (the daemon hashes the bytes with
 * BLAKE3 and stores `digest + mime + filename + size`); it only needs to
 * change whenever the reference changes. The field set deliberately differs
 * from the VS Code client's key (mime + filename + size + bytes): this client
 * also tags the LOCAL SOURCE KIND (image / document / in-memory binary), so a
 * reference that moves between local source categories re-uploads instead of
 * reusing a stale durable id; the VS Code client has only binary attachments
 * and therefore no kind tag. A rename/re-select of identical bytes is a
 * DIFFERENT reference and uploads fresh, while the same reference at a
 * different position/list order reuses its retained id.
 */
internal fun pathUploadKey(
    kind: String,
    mime: String,
    filename: String?,
    bytes: ByteArray
): String =
    kind + ":" + mime + ":name=" + (filename ?: "") + ":size=" + bytes.size +
        ":sha=" + sha256Hex(bytes)

/** Lowercase SHA-256 of the exact bytes (the retry identity of a binary). */
private fun sha256Hex(bytes: ByteArray): String {
    val digest = java.security.MessageDigest.getInstance("SHA-256").digest(bytes)
    val out = StringBuilder(digest.size * 2)
    for (b in digest) {
        out.append(HEX[(b.toInt() ushr 4) and 0x0f])
        out.append(HEX[b.toInt() and 0x0f])
    }
    return out.toString()
}

private const val HEX = "0123456789abcdef"

/** The ActionMap key of the paste binding shared by every focus target. */
private const val PASTE_ACTION = "faktor-paste-image"

/**
 * The platform menu-shortcut mask: Command on macOS, Ctrl on Windows/Linux,
 * decided by the toolkit and never hardcoded. A headless JVM has no
 * toolkit display, so the mask falls back to the documented Ctrl/X11
 * contract instead of throwing HeadlessException; the paste binding always
 * exists, headless included.
 */
internal fun menuShortcutMask(): Int =
    if (GraphicsEnvironment.isHeadless()) {
        InputEvent.CTRL_DOWN_MASK
    } else {
        Toolkit.getDefaultToolkit().menuShortcutKeyMaskEx
    }

/**
 * The ONE platform menu-shortcut paste stroke: `KeyStroke.getKeyStroke(
 * KeyEvent.VK_V, menuShortcutMask())` — Command+V on macOS, Ctrl+V on
 * Windows/Linux (and the headless fallback), never a hardcoded Ctrl stroke.
 */
private fun menuShortcutV(): KeyStroke =
    KeyStroke.getKeyStroke(KeyEvent.VK_V, menuShortcutMask())

/**
 * Bounded LOCAL pending-upload state for the Task composer (audit 29,
 * parity with the VS Code pending-submission envelope): after a successful
 * upload the COMPLETE durable reference (`ref_id` included) is retained
 * here, bound to the session AND the exact reference key ([pathUploadKey]:
 * kind + mime + filename + size + byte digest) it was uploaded under; a
 * start failure keeps the state, and the retry resolves the reference first
 * and uploads only the absent ones (CAS dedupe foundation). A
 * renamed/re-selected file computes a new key and uploads fresh. An entry
 * retained for one session is never reused by another session, and the map
 * is bounded so a hostile client can never grow host memory with submission
 * identities.
 */
class PendingAttachmentRetry(private val maxEntries: Int = MAX_PENDING_UPLOADS) {

    /** One already-uploaded attachment retained in local pending state: the
     *  complete typed reference, `ref_id` included. Task admission receives
     *  its exact [attachmentId] projection. */
    data class RetainedAttachment(
        val sessionId: String,
        val key: String,
        val attachment: ProtocolAttachmentRef
    )

    private val retained = LinkedHashMap<String, RetainedAttachment>()

    /** The durable reference retained for this exact (session, key), or null. */
    fun reusable(sessionId: String, key: String): ProtocolAttachmentRef? =
        retained[mapKey(sessionId, key)]?.attachment

    /** Retain one successful upload; the oldest entry is evicted past the bound. */
    fun retain(sessionId: String, key: String, attachment: ProtocolAttachmentRef) {
        val mapKey = mapKey(sessionId, key)
        retained.remove(mapKey)
        retained[mapKey] = RetainedAttachment(sessionId, key, attachment)
        while (retained.size > maxEntries) {
            val oldest = retained.keys.firstOrNull() ?: break
            retained.remove(oldest)
        }
    }

    /**
     * Resolve the task-admission `AttachmentId`s for one pending submission
     * in ENTRY ORDER: already-uploaded entries resolve first (projected from
     * the retained reference) and `upload(index)` runs ONLY for the absent
     * ones. Every successful upload is retained immediately, so a later start
     * failure keeps it and the retry uploads nothing twice.
     */
    fun resolve(
        sessionId: String,
        keys: List<String>,
        upload: (Int) -> ProtocolAttachmentRef
    ): List<NativeAttachmentId> {
        val ids = ArrayList<NativeAttachmentId>(keys.size)
        for (index in keys.indices) {
            val key = keys[index]
            val existing = reusable(sessionId, key)
            if (existing != null) {
                ids.add(existing.attachmentId())
                continue
            }
            val uploaded = upload(index)
            retain(sessionId, key, uploaded)
            ids.add(uploaded.attachmentId())
        }
        return ids
    }

    /** Durable acceptance: the retained ids for these entries are no longer needed. */
    fun release(sessionId: String, keys: List<String>) {
        for (key in keys) retained.remove(mapKey(sessionId, key))
    }

    fun size(): Int = retained.size

    private fun mapKey(sessionId: String, key: String): String = sessionId + "\u0000" + key

    companion object {
        /** Bounded state: at most this many retained uploads survive. */
        const val MAX_PENDING_UPLOADS = 64
    }
}

/**
 * The exact task-admission projection of one durable reference: the
 * `AttachmentId` a task start carries, never widened with `ref_id` (the
 * reference identity is retained locally).
 */
fun ProtocolAttachmentRef.attachmentId(): NativeAttachmentId =
    NativeAttachmentId(digest, mime, filename, size)

/**
 * The Task composer's attachment list. [workspaceRoot] is the SESSION
 * workspace root the picked/dropped paths are relativized against (the
 * IntelliJ host passes `Project.basePath`, the standalone launcher its
 * working directory); the same root rides session creation, so the daemon
 * resolves the submitted `files` against it. A path outside the root (or
 * any path when the root is unknown) is refused typed and shown in the
 * refusal list — visible, never silently dropped and never sent absolute.
 */
class AttachmentsPanel(private val workspaceRoot: Path? = null) : JPanel(BorderLayout()) {

    private val paths = DefaultListModel<String>()

    private val list = JList(paths)

    /** Typed refusals of entries that never entered [paths]. */
    private val refusals = DefaultListModel<AttachmentRefusal>()

    private val refusalList = JList(refusals)

    private val refusalScroll = JScrollPane(refusalList)

    /** In-memory binary attachments (clipboard images): NEVER filesystem paths. */
    private val binaries = DefaultListModel<PendingBinaryAttachment>()

    private val binaryList = JList(binaries)

    private val binaryScroll = JScrollPane(binaryList)

    private val addButton = JButton("Add files...")

    private val pasteButton = JButton("Paste image")

    private val removeButton = JButton("Remove")

    private val clearButton = JButton("Clear")

    /** One-line notice sink for the host transcript (optional in tests). */
    var onNotice: ((String) -> Unit)? = null

    init {
        list.selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
        list.toolTipText = "drop files here to attach them to the task"
        refusalList.selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
        refusalList.toolTipText = "refused attachments (outside the session workspace)"
        refusalScroll.preferredSize = Dimension(0, 48)
        refusalScroll.isVisible = false
        binaryList.selectionMode = ListSelectionModel.MULTIPLE_INTERVAL_SELECTION
        binaryList.toolTipText = "clipboard images staged in memory (uploaded as durable attachments)"
        binaryScroll.preferredSize = Dimension(0, 48)
        binaryScroll.isVisible = false
        val handler = attachmentTransferHandler()
        list.transferHandler = handler
        binaryList.transferHandler = handler

        val buttons = JPanel(FlowLayout(FlowLayout.LEFT, 4, 0))
        buttons.add(addButton)
        buttons.add(pasteButton)
        buttons.add(removeButton)
        buttons.add(clearButton)

        addButton.addActionListener { chooseFiles() }
        pasteButton.addActionListener { requestPasteFromClipboard() }
        removeButton.addActionListener {
            val selected = list.selectedValuesList
            for (path in selected) paths.removeElement(path)
            val selectedRefusals = refusalList.selectedValuesList
            for (refusal in selectedRefusals) refusals.removeElement(refusal)
            val selectedBinaries = binaryList.selectedValuesList
            for (binary in selectedBinaries) binaries.removeElement(binary)
            updateRefusalVisibility()
            updateBinaryVisibility()
        }
        clearButton.addActionListener { clear() }
        bindPaste(list)
        bindPaste(binaryList)
        bindPaste(this)

        val body = JPanel(BorderLayout(0, 2))
        body.add(JScrollPane(list), BorderLayout.CENTER)
        val south = JPanel(BorderLayout())
        south.add(refusalScroll, BorderLayout.NORTH)
        south.add(binaryScroll, BorderLayout.CENTER)
        south.add(buttons, BorderLayout.SOUTH)
        body.add(south, BorderLayout.SOUTH)
        add(body, BorderLayout.CENTER)
    }

    /** The attached workspace-relative paths, in list order (the task-run `files` array). */
    fun files(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until paths.size()) out.add(paths.getElementAt(i))
        return out
    }

    /** The typed refusals of entries that stayed out of [files], in order. */
    fun refusals(): List<AttachmentRefusal> {
        val out = ArrayList<AttachmentRefusal>()
        for (i in 0 until refusals.size()) out.add(refusals.getElementAt(i))
        return out
    }

    /** The refused-entry count (visible in the refusal list, never staged). */
    fun refusalCount(): Int = refusals.size()

    /** The in-memory binary attachments, in staging order (images only today). */
    fun binaryAttachments(): List<PendingBinaryAttachment> {
        val out = ArrayList<PendingBinaryAttachment>()
        for (i in 0 until binaries.size()) out.add(binaries.getElementAt(i))
        return out
    }

    /**
     * Stage picked/dropped paths: each entry is relativized against the
     * session workspace root and held as a workspace-relative path. An entry
     * outside the root, or any entry when no root is known, becomes a typed
     * [AttachmentRefusal] recorded in the refusal list (and announced via
     * [onNotice]) — never silently dropped and never staged as absolute.
     */
    fun addFiles(pathsToAdd: List<String>) {
        for (path in pathsToAdd) {
            val relative = try {
                workspaceRelativePath(path, workspaceRoot)
            } catch (e: AttachmentRefusal) {
                refusals.addElement(e)
                onNotice?.invoke("attachment refused: ${e.message}")
                updateRefusalVisibility()
                continue
            }
            if (!contains(relative)) paths.addElement(relative)
        }
    }

    /**
     * Stage one in-memory binary attachment. Bounded: an empty payload or
     * one above the emergency upload ceiling is refused (false), so a
     * hostile caller can never stage unbounded bytes.
     */
    fun addBinary(mime: String, filename: String?, bytes: ByteArray): Boolean {
        if (bytes.isEmpty() || bytes.size.toLong() > AttachmentImages.MAX_UPLOAD_BYTES) return false
        binaries.addElement(PendingBinaryAttachment(mime, filename, bytes))
        updateBinaryVisibility()
        return true
    }

    /**
     * Stage one clipboard image as bounded PNG bytes converted IN MEMORY
     * (no filesystem path, no temp file). False when the conversion is
     * refused (oversize bytes or pixel bound).
     */
    fun addClipboardImage(image: Image, filename: String = "clipboard.png"): Boolean {
        val bytes = AttachmentImages.pngBytes(image) ?: return false
        return addBinary("image/png", filename, bytes)
    }

    /**
     * Paste the system clipboard image (if any) as an in-memory pending
     * attachment. Returns null on success, else the typed refusal text the
     * composer surfaces; a headless/absent clipboard is never an exception
     * and never a silent no-op: headless returns the explicit unavailable
     * refusal before any toolkit access.
     */
    fun pasteImageFromClipboard(): String? {
        if (GraphicsEnvironment.isHeadless()) {
            return "the system clipboard is unavailable in this environment"
        }
        val clipboard: Clipboard = try {
            Toolkit.getDefaultToolkit().systemClipboard
        } catch (e: Exception) {
            return "the system clipboard is unavailable in this environment"
        }
        if (!clipboard.isDataFlavorAvailable(DataFlavor.imageFlavor)) {
            return "the clipboard holds no image"
        }
        val image = try {
            clipboard.getData(DataFlavor.imageFlavor) as? Image
        } catch (e: Exception) {
            null
        } ?: return "the clipboard image could not be read"
        return if (addClipboardImage(image)) {
            null
        } else {
            "the clipboard image is refused: it exceeds the bounded PNG conversion " +
                "(${AttachmentImages.MAX_IMAGE_BYTES} bytes / " +
                "${AttachmentImages.MAX_CLIPBOARD_PIXELS} pixels)"
        }
    }

    /** The pasted-image count (separate from the file-path count). */
    fun binaryCount(): Int = binaries.size()

    /** Paths + staged binaries (the composer's attachment total). */
    fun count(): Int = paths.size() + binaries.size()

    fun clear() {
        paths.clear()
        refusals.clear()
        binaries.clear()
        updateRefusalVisibility()
        updateBinaryVisibility()
    }

    private fun requestPasteFromClipboard() {
        val refusal = pasteImageFromClipboard()
        if (refusal != null) {
            onNotice?.invoke(refusal)
        } else {
            onNotice?.invoke(
                "clipboard image attached in memory (image/png, " +
                    (binaries.lastElementOrNull()?.size() ?: 0) +
                    " bytes); it uploads as a durable attachment on start"
            )
        }
    }

    private fun attachmentTransferHandler(): TransferHandler = object : TransferHandler() {
        override fun canImport(support: TransferSupport): Boolean =
            support.isDataFlavorSupported(DataFlavor.javaFileListFlavor) ||
                support.isDataFlavorSupported(DataFlavor.imageFlavor)

        override fun importData(support: TransferSupport): Boolean {
            if (support.isDataFlavorSupported(DataFlavor.javaFileListFlavor)) {
                @Suppress("UNCHECKED_CAST")
                val files = support.transferable
                    .getTransferData(DataFlavor.javaFileListFlavor) as List<File>
                addFiles(files.map { it.absolutePath })
                return true
            }
            if (support.isDataFlavorSupported(DataFlavor.imageFlavor)) {
                val image = try {
                    support.transferable.getTransferData(DataFlavor.imageFlavor) as? Image
                } catch (e: Exception) {
                    null
                }
                if (image == null) {
                    onNotice?.invoke("the dropped image could not be read")
                    return false
                }
                if (!addClipboardImage(image)) {
                    onNotice?.invoke(
                        "the dropped image is refused: it exceeds the bounded PNG conversion"
                    )
                    return false
                }
                onNotice?.invoke(
                    "image attached in memory (image/png, " +
                        (binaries.lastElementOrNull()?.size() ?: 0) +
                        " bytes); it uploads as a durable attachment on start"
                )
                return true
            }
            return false
        }
    }

    /**
     * The platform menu shortcut pastes an image from the clipboard wherever
     * this panel has focus: Command+V on macOS, Ctrl+V on Windows/Linux —
     * the toolkit's own menu-shortcut mask, never a hardcoded Ctrl.
     */
    private fun bindPaste(target: JComponent) {
        val action = object : AbstractAction(PASTE_ACTION) {
            override fun actionPerformed(e: ActionEvent?) {
                requestPasteFromClipboard()
            }
        }
        target.actionMap.put(PASTE_ACTION, action)
        target.inputMap.put(menuShortcutV(), PASTE_ACTION)
    }

    /**
     * The paste binding actually installed on this panel (Command+V on
     * macOS, Ctrl+V elsewhere), exposed so tests assert the platform
     * abstraction without assuming one platform's mask.
     */
    internal fun pasteKeyStroke(): KeyStroke? =
        inputMap.allKeys().firstOrNull { stroke -> inputMap.get(stroke) == PASTE_ACTION }

    private fun updateBinaryVisibility() {
        binaryScroll.isVisible = binaries.size() > 0
        revalidate()
        repaint()
    }

    private fun updateRefusalVisibility() {
        refusalScroll.isVisible = refusals.size() > 0
        revalidate()
        repaint()
    }

    private fun DefaultListModel<PendingBinaryAttachment>.lastElementOrNull(): PendingBinaryAttachment? =
        if (size() == 0) null else getElementAt(size() - 1)

    private fun chooseFiles() {
        val chooser = JFileChooser()
        chooser.isMultiSelectionEnabled = true
        chooser.fileSelectionMode = JFileChooser.FILES_ONLY
        if (chooser.showOpenDialog(this) == JFileChooser.APPROVE_OPTION) {
            val selected = chooser.selectedFiles ?: emptyArray()
            addFiles(selected.map { it.absolutePath })
        }
    }

    private fun contains(path: String): Boolean {
        for (i in 0 until paths.size()) {
            if (paths.getElementAt(i) == path) return true
        }
        return false
    }
}
