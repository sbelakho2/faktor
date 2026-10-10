// Coordination: the durable run-family board of the current session as
// served by the additive native route (`GET /native/session/{id}/board`),
// rendered as human activity — who posted, what they said and when — plus
// one bounded post composer (`POST .../board`). Revision numbers, author ids
// and evidence refs stay behind the Advanced disclosure; the real
// "Load older" paging contract is unchanged. When the serving daemon
// exposes no board route the panel records an explicit unavailable state
// with the typed reason — it never fabricates posts or unread counts.
// Pure presentation: reads and posts are delegated to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.NativeBoardPage
import dev.faktor.shared.NativeBoardPost
import java.awt.BorderLayout
import javax.swing.BorderFactory
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.JTextField

// Wire bounds are UTF-8 BYTES (the daemon refuses on bytes, and the VS Code
// panel gates on bytes): counting UTF-16 chars accepted a 512-emoji subject
// that the daemon then refused.
private const val MAX_BOARD_SUBJECT_BYTES = 512
private const val MAX_BOARD_BODY_BYTES = 16 * 1024
private const val MAX_BOARD_LINE_CHARS = 240

/**
 * The bounded local window of loaded posts: Load older keeps paging while
 * the daemon says `has_more`, but the local transcript stops at this many
 * posts and the header says so instead of growing without bound.
 */
private const val MAX_BOARD_POSTS = 500

/** The board's human empty state (one sentence; never a fabricated post). */
private const val NO_BOARD_POSTS = "No posts yet. Notes and handoffs appear here."

class BoardPanel : JPanel(BorderLayout()) {

    interface Listener {
        /** Explicit read: acknowledges the current top of the board. */
        fun onRead()

        /** One bounded post; engine refusals stay typed and visible. */
        fun onPost(subject: String, body: String)

        /** Loads the next OLDER page before [beforeRevision]; default no-op. */
        fun onLoadOlder(beforeRevision: Long) {}
    }

    private val header = WrappedLabel("coordination: not read yet")

    private val postsArea = compactArea(10)

    private val advancedArea = compactArea(4, monospace = true)

    private val subjectField = JTextField(18)

    private val bodyArea = JTextArea(2, 18)

    private val postButton = primaryButton("Post")

    private val readButton = secondaryButton("Read board")

    private val loadOlderButton = secondaryButton("Load older")

    private val advancedToggle = secondaryButton("Show advanced")

    private val advancedBody = JPanel(BorderLayout())

    private val composer = ScrollableColumn()

    private var listener: Listener? = null

    private var available = false

    private var seenRevision: Long = 0L

    private var latestRevision: Long = 0L

    private var unread: Int = 0

    private var loadedPosts: Int = 0

    private var nextBeforeRevision: Long? = null

    private var hasMore: Boolean = false

    private var pagingNote: String? = null

    init {
        bodyArea.lineWrap = true
        bodyArea.wrapStyleWord = true
        bodyArea.font = uiPanelFont()
        postButton.isEnabled = false
        readButton.isEnabled = false
        loadOlderButton.isEnabled = false
        postButton.addActionListener { submitComposer() }
        readButton.addActionListener { if (readButton.isEnabled) listener?.onRead() }
        loadOlderButton.addActionListener { loadOlder() }
        val draftListener = object : javax.swing.event.DocumentListener {
            override fun insertUpdate(e: javax.swing.event.DocumentEvent?) = updateComposer()
            override fun removeUpdate(e: javax.swing.event.DocumentEvent?) = updateComposer()
            override fun changedUpdate(e: javax.swing.event.DocumentEvent?) = updateComposer()
        }
        subjectField.document.addDocumentListener(draftListener)
        bodyArea.document.addDocumentListener(draftListener)

        val subjectRow = JPanel(BorderLayout(Spacing.S, 0))
        subjectRow.isOpaque = false
        subjectRow.add(mutedLabel("Subject"), BorderLayout.WEST)
        // The Read control lives with the paging actions (below): keeping it
        // in this row collapsed the subject field to a sliver at 240px.
        subjectRow.add(subjectField, BorderLayout.CENTER)
        val bodyLabel = mutedLabel("Message")
        val bodyScroll = insetScroll(bodyArea)
        val pagingHint = wrappedMutedLabel(
            "Older posts load on demand; the local window is bounded."
        )
        val postRow = actionRow(postButton)
        composer.border = BorderFactory.createEmptyBorder(0, 0, 0, 0)
        composer.add(subjectRow)
        composer.add(vSpace(Spacing.S))
        composer.add(bodyLabel)
        composer.add(vSpace(Spacing.XS))
        composer.add(bodyScroll)
        composer.add(vSpace(Spacing.S))
        composer.add(actionRow(loadOlderButton, readButton))
        composer.add(vSpace(Spacing.XS))
        composer.add(pagingHint)
        composer.add(vSpace(Spacing.S))
        composer.add(postRow)

        advancedToggle.addActionListener {
            advancedBody.isVisible = advancedToggle.isSelected
            advancedToggle.text = if (advancedToggle.isSelected) {
                "Hide advanced"
            } else {
                "Show advanced"
            }
            revalidate()
            repaint()
        }
        advancedBody.isOpaque = false
        advancedBody.add(insetScroll(advancedArea), BorderLayout.CENTER)
        advancedBody.isVisible = false

        val body = pageColumn()
        val postsBody = JPanel(BorderLayout())
        postsBody.isOpaque = false
        postsBody.add(insetScroll(postsArea), BorderLayout.CENTER)
        body.add(card("Agent activity", postsBody))
        body.add(vSpace(Spacing.M))
        body.add(advancedToggle)
        body.add(vSpace(Spacing.XS))
        body.add(advancedBody)
        body.add(vSpace(Spacing.M))
        body.add(card("New post", composer))

        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        setAvailable(false)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /**
     * Renders one validated page. `acknowledge` is true only for an explicit
     * read: it moves the read watermark so the next render reports 0 unread.
     * Automatic refreshes never mark posts read.
     */
    fun setBoard(page: NativeBoardPage, acknowledge: Boolean = false) {
        setAvailable(true)
        if (acknowledge) {
            seenRevision = maxOf(seenRevision, page.revision)
        }
        latestRevision = page.revision
        unread = page.posts.count { it.revision > seenRevision }
        loadedPosts = page.posts.size
        nextBeforeRevision = page.nextBeforeRevision
        hasMore = page.hasMore
        pagingNote = null
        postsArea.text = humanPosts(page.posts)
        advancedArea.text = advancedPosts(page.posts)
        renderHeader()
        updateLoadOlder()
    }

    /**
     * Appends the next OLDER page (Load older). The local window is bounded:
     * once [MAX_BOARD_POSTS] posts are loaded, the control disables itself
     * and the header names the bound instead of the transcript growing
     * without limit.
     */
    fun appendOlderPage(page: NativeBoardPage) {
        setAvailable(true)
        val human = StringBuilder()
        if (loadedPosts == 0 && postsArea.text == NO_BOARD_POSTS) {
            postsArea.text = ""
        } else {
            human.append(postsArea.text)
            if (human.isNotEmpty() && human.last() != '\n') human.append('\n')
        }
        val chunk = humanPosts(page.posts)
        if (chunk.isNotEmpty()) human.append(chunk)
        unread += page.posts.count { it.revision > seenRevision }
        loadedPosts += page.posts.size
        nextBeforeRevision = page.nextBeforeRevision
        hasMore = page.hasMore
        pagingNote = if (hasMore && loadedPosts >= MAX_BOARD_POSTS) {
            "local window bound reached ($MAX_BOARD_POSTS posts); older pages remain on the service"
        } else {
            null
        }
        postsArea.text = human.toString()
        val advanced = StringBuilder(advancedArea.text)
        if (advanced.isNotEmpty() && advanced.last() != '\n') advanced.append('\n')
        advanced.append(advancedPosts(page.posts))
        advancedArea.text = advanced.toString()
        renderHeader()
        updateLoadOlder()
    }

    /** Human activity lines: author and time first, then the message. */
    private fun humanPosts(posts: List<NativeBoardPost>): String {
        if (posts.isEmpty()) return ""
        val text = StringBuilder()
        for (post in posts) {
            text.append(authorLabel(post)).append(" · ").append(historyTimeText(post.createdMs))
            text.append('\n')
            text.append(bound(post.subject, MAX_BOARD_LINE_CHARS))
            text.append('\n')
            text.append(bound(post.body, MAX_BOARD_LINE_CHARS))
            text.append('\n')
        }
        return text.toString()
    }

    /** Advanced raw lines: revision, author id and evidence refs. */
    private fun advancedPosts(posts: List<NativeBoardPost>): String {
        if (posts.isEmpty()) return ""
        val text = StringBuilder()
        for (post in posts) {
            val author = post.authorChild?.let { "child:$it" } ?: "root"
            text.append("revision ").append(post.revision).append(" [").append(author).append("]")
            if (post.refs.isNotEmpty()) {
                text.append(" refs=").append(post.refs.joinToString(",", limit = 5))
            }
            text.append('\n')
        }
        return text.toString()
    }

    private fun authorLabel(post: NativeBoardPost): String =
        post.authorChild?.let { "Agent $it" } ?: "Conversation"

    private fun renderHeader() {
        header.text = "coordination: " + plural(loadedPosts, "post") +
            " · $unread unread" +
            (if (hasMore) " (older pages exist)" else "") +
            (pagingNote?.let { " — $it" } ?: "")
    }

    /** Pages one bounded older window when the daemon says more exists. */
    fun loadOlder() {
        if (!loadOlderButton.isEnabled) return
        val cursor = nextBeforeRevision ?: return
        listener?.onLoadOlder(cursor)
    }

    private fun updateLoadOlder() {
        loadOlderButton.isEnabled = available && hasMore &&
            nextBeforeRevision != null && loadedPosts < MAX_BOARD_POSTS
    }

    /** A typed failure while paging: stays visible, never fabricated posts. */
    fun setPagingNote(text: String) {
        pagingNote = bound(text, MAX_BOARD_LINE_CHARS)
        renderHeader()
    }

    /**
     * Explicit unavailable state: the serving daemon has no board read (or
     * the read failed). The typed reason is recorded verbatim; no posts are
     * ever fabricated and the composer is disabled.
     */
    fun setUnavailable(reason: String) {
        setAvailable(false)
        latestRevision = 0L
        unread = 0
        loadedPosts = 0
        nextBeforeRevision = null
        hasMore = false
        pagingNote = null
        header.text = "coordination: unavailable (" + bound(reason, MAX_BOARD_LINE_CHARS) + ")"
        postsArea.text = ""
        advancedArea.text = ""
        loadOlderButton.isEnabled = false
    }

    /** Clears to the pre-read state (daemon stopped / session switched). */
    fun reset() {
        seenRevision = 0L
        latestRevision = 0L
        unread = 0
        loadedPosts = 0
        nextBeforeRevision = null
        hasMore = false
        pagingNote = null
        setAvailable(false)
        header.text = "coordination: not read yet"
        postsArea.text = ""
        advancedArea.text = ""
        loadOlderButton.isEnabled = false
    }

    fun clearComposer() {
        subjectField.text = ""
        bodyArea.text = ""
    }

    fun available(): Boolean = available

    fun headerText(): String = header.fullText

    /** The rendered human activity text. */
    fun postsText(): String = postsArea.text

    /** The rendered advanced revision/ref text. */
    fun advancedText(): String = advancedArea.text

    fun postEnabled(): Boolean = postButton.isEnabled

    fun readEnabled(): Boolean = readButton.isEnabled

    fun loadOlderEnabled(): Boolean = loadOlderButton.isEnabled

    fun loadOlderCursor(): Long? = nextBeforeRevision

    fun postsBoundReached(): Boolean = loadedPosts >= MAX_BOARD_POSTS

    fun subject(): String = subjectField.text.trim()

    fun body(): String = bodyArea.text

    fun composerVisible(): Boolean = composer.parent != null

    /** Test/composer hook: fills the composer without a display. */
    fun setComposerFields(subject: String, body: String) {
        subjectField.text = subject
        bodyArea.text = body
        updateComposer()
    }

    /** The local draft refusal (the same byte bounds the daemon enforces). */
    fun draftRefusal(): String? {
        val subject = subjectField.text.trim()
        val body = bodyArea.text
        if (subject.isEmpty()) return "subject is required"
        if (utf8Bytes(subject) > MAX_BOARD_SUBJECT_BYTES) {
            return "subject exceeds $MAX_BOARD_SUBJECT_BYTES bytes"
        }
        if (body.trim().isEmpty()) return "message is required"
        if (utf8Bytes(body) > MAX_BOARD_BODY_BYTES) {
            return "message exceeds $MAX_BOARD_BODY_BYTES bytes"
        }
        return null
    }

    private fun utf8Bytes(text: String): Int = text.toByteArray(Charsets.UTF_8).size

    /**
     * The Post action exactly as the button runs it. Posting is possible only
     * when the local byte-bounded draft is valid; the submitted text is NOT
     * silently truncated (a truncated post would alter the operator's words).
     */
    fun submitComposer() {
        if (!postButton.isEnabled) return
        listener?.onPost(subjectField.text.trim(), bodyArea.text)
    }

    private fun updateComposer() {
        postButton.isEnabled = available && draftRefusal() == null
    }

    private fun setAvailable(value: Boolean) {
        available = value
        readButton.isEnabled = value
        updateComposer()
    }
}
