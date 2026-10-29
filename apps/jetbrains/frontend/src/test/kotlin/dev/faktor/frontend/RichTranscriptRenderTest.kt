// TEST-SEPARATION: oracle=contract isolation=pure-unit
//
// Adversarial tests for the rich transcript renderer (audit item 7 + the
// You/Faktor and logical-turn parts of item 6). The ORACLE is the renderer
// contract itself — bounded markdown structure, safe-link allowlist,
// incremental tail rendering with surviving component identity, stable
// You/Faktor identities, and turn grouping — expressed as explicit
// assertions; the SUT never grades itself.
//
// Every case tries to break the boundary:
//  - hostile markdown: pipe bombs, path-traversal fence languages, HTML
//    injection, unclosed fences, oversized single blocks, deep emphasis;
//  - unsafe link schemes (javascript:, file:, ide:, data:) must never
//    become interactive, and the clipboard/browser seams must stay
//    headless-safe;
//  - a growing live message must NOT re-create settled block components,
//    and a divergence must rebuild instead of corrupting;
//  - grouping: a user row splits a run, a live turn id subdivides it, and
//    the plain-text projection carries "You:"/"Faktor:" exactly once per
//    turn group;
//  - the bounded sink: trimmed rows are tombstoned and can never be
//    re-inserted by a late page.
//
// Isolation: pure unit — no daemon, no sockets, no developer ~/.faktor.
package dev.faktor.frontend

import dev.faktor.backend.LiveChunkFrame
import dev.faktor.shared.NativeMessage
import java.awt.Container
import javax.swing.JEditorPane
import org.junit.Test

class RichTranscriptRenderTest {

    // ------------------------------------------------- parser adversarial

    @Test
    fun hostileMarkdownIsBoundedAndNeverHtml() {
        // Pipe bomb: columns cap at 16, rows at 128 — no widget can grow.
        val wideHeader = (1..200).joinToString("|") { "c$it" }
        val wideSeparator = (1..200).joinToString("|") { "---" }
        val tableBomb = wideHeader + "\n" + wideSeparator + "\n" +
            (1..500).joinToString("\n") { (1..200).joinToString("|") { "v$it" } }
        val table = RichMarkdown.parse(tableBomb).filterIsInstance<MarkdownBlock.Table>().single()
        check(table.header.size <= RichMarkdown.MAX_TABLE_COLUMNS) { "columns ${table.header.size}" }
        check(table.rows.size <= RichMarkdown.MAX_TABLE_ROWS) { "rows ${table.rows.size}" }

        // Path-traversal "language": the fence class excludes `/`, so the
        // hostile header never yields a Code block whose language is a path
        // — at worst the stray closing fence parses as an empty generic
        // block, and the header itself stays visible as literal text.
        val traversal = RichMarkdown.parse("```../../etc/passwd\nrm -rf /\n```")
        val traversalCodes = traversal.filterIsInstance<MarkdownBlock.Code>()
        check(traversalCodes.all { code ->
            code.language.isEmpty() || !code.language.contains('/')
        }) { "hostile fence languages: $traversalCodes" }
        check(
            traversal.joinToString("\n") { it.plainText() }
                .contains("../../etc/passwd"),
            "the hostile header stays visible as literal text: $traversal"
        )

        // HTML injection is only ever characters.
        val html = RichMarkdown.parse("<img src=x onerror=alert(1)>\n<script>evil()</script>")
        check(html.all { it is MarkdownBlock.Paragraph }) { "$html" }
        check(html.single().plainText().contains("<script>evil()</script>")) { "$html" }

        // Deep emphasis nesting is depth-capped, never stack-poisoned.
        val deep = "*a *b *c *d *e *f *g *h* f* e* d* c* b* a*"
        val deepBlocks = RichMarkdown.parse(deep)
        check(deepBlocks.isNotEmpty())

        // Oversized single block:1MB of one paragraph parses in bounded
        // spans and terminates.
        val huge = "x".repeat(1_000_000)
        val hugeBlocks = RichMarkdown.parse(huge)
        check(hugeBlocks.size == 1) { "one block, got ${hugeBlocks.size}" }
        val hugeSpans = (hugeBlocks.single() as MarkdownBlock.Paragraph).spans
        check(hugeSpans.size <= RichMarkdown.MAX_SPANS) { "spans ${hugeSpans.size}" }

        // Unclosed fence: one Code block, unsettled (the tail stays live).
        val unclosed = "```kotlin\nval x ="
        val unclosedBlocks = RichMarkdown.parse(unclosed)
        check(unclosedBlocks.single() is MarkdownBlock.Code) { "$unclosedBlocks" }
        check(RichMarkdown.settledEnd(unclosed) == 0) {
            "an unclosed fence must never settle"
        }
    }

    @Test
    fun settledEndBoundariesMatchBlockCompletion() {
        check(RichMarkdown.settledEnd("# Title\n") == "# Title\n".length) {
            "a heading settles on its own line"
        }
        val partial = "a paragraph still streaming"
        check(RichMarkdown.settledEnd(partial) == 0) {
            "a paragraph settles only after a blank line"
        }
        val completed = "a paragraph\n\nnext"
        check(RichMarkdown.settledEnd(completed) == "a paragraph\n\n".length) {
            "the blank line settles the paragraph"
        }
        val closedFence = "```sh\necho hi\n```\nafter"
        check(RichMarkdown.settledEnd(closedFence) == "```sh\necho hi\n```\n".length) {
            "the closing fence settles the code block"
        }
        check(RichMarkdown.settledEnd("---\nafter") == "---\n".length) {
            "a rule settles itself"
        }
    }

    @Test
    fun unsafeSchemesNeverBecomeLinks() {
        check(RichMarkdown.safeUrl("https://example.com/a?b=c") != null)
        check(RichMarkdown.safeUrl("http://example.com") != null)
        check(RichMarkdown.safeUrl("mailto:dev@example.com") != null)
        check(RichMarkdown.safeUrl("javascript:alert(1)") == null)
        check(RichMarkdown.safeUrl("JAVASCRIPT:alert(1)") == null)
        check(RichMarkdown.safeUrl("file:///etc/passwd") == null)
        check(RichMarkdown.safeUrl("ide://run-command?x=1") == null)
        check(RichMarkdown.safeUrl("command:rm -rf /") == null)
        check(RichMarkdown.safeUrl("data:text/html,<script>") == null)
        check(RichMarkdown.safeUrl("https://") == null, "host-less https is refused")
        check(RichMarkdown.safeUrl("mailto:not-an-email") == null)
        check(RichMarkdown.safeUrl("https://ex\u0000ample.com") == null)
        check(RichMarkdown.safeUrl("https://example.com".repeat(500)) == null)

        val parsed = RichMarkdown.parse(
            "[a](javascript:alert(1)) [b](file:///etc) [c](ide://x) [ok](https://example.com)"
        )
        val links = (parsed.single() as MarkdownBlock.Paragraph).spans
            .filterIsInstance<InlineSpan.Link>()
        check(links.size == 1 && links.single().url == "https://example.com") {
            "only the safe scheme survives: $links"
        }
        // The unsafe URLs remain VISIBLE as literal characters.
        val plain = parsed.single().plainText()
        check(plain.contains("javascript:alert(1)")) { plain }
    }

    // ------------------------------------- incremental rendering identity

    @Test
    fun settledBlocksKeepTheirComponentIdentityWhileTheTailGrows() {
        val body = AssistantBody(uiPanelFont())
        body.update("Hello wor")
        val tail1 = body.tailComponentsForTest().single()
        check(body.settledComponentsForTest().isEmpty()) { "nothing settled yet" }

        // The same block grows: the tail component is refreshed IN PLACE.
        body.update("Hello world")
        check(body.tailComponentsForTest().single() === tail1) {
            "a growing tail must keep its component identity"
        }

        // The blank line settles the paragraph — the dominant streaming
        // case KEEPS the exact component.
        body.update("Hello world\n\n")
        val settled = body.settledComponentsForTest().single()
        check(settled === tail1) { "the settling tail component must survive" }

        // Later content never touches the settled block again.
        body.update("Hello world\n\nMore text streams here")
        val settledAfter = body.settledComponentsForTest().single()
        check(settledAfter === settled) { "a settled block must never re-render" }
        check(body.tailComponentsForTest().size == 1)
        check(body.tailComponentsForTest().single() !== settled)
        check(body.renderedText == "Hello world\n\nMore text streams here")

        // A heading settles immediately and survives the paragraph growth.
        val headed = AssistantBody(uiPanelFont())
        headed.update("# T\n\npara one")
        val heading = headed.settledComponentsForTest().single()
        check(headed.settledComponentsForTest().size == 1)
        headed.update("# T\n\npara one grows")
        check(headed.settledComponentsForTest().single() === heading) {
            "the heading component must survive tail growth"
        }
    }

    @Test
    fun nonAppendDivergenceRebuildsInsteadOfCorrupting() {
        val body = AssistantBody(uiPanelFont())
        body.update("first lineage\n\n")
        check(body.settledComponentsForTest().size == 1)
        // A snapshot divergence REPLACES the row text: the settled prefix no
        // longer matches and the body rebuilds instead of splicing.
        body.update("diverged content")
        check(body.renderedText == "diverged content")
        check(body.settledComponentsForTest().isEmpty()) { "the stale settled prefix is dropped" }
        val pane = body.tailComponentsForTest().single() as RichTextPane
        check(pane.text.contains("diverged content")) { pane.text }
        check(!pane.text.contains("first lineage")) { pane.text }
        assertNoHtmlComponents(body)
    }

    @Test
    fun hostileGrowthNeverUnboundsTheComponentTree() {
        val body = AssistantBody(uiPanelFont())
        val hostile = (1..5_000).joinToString("\n") { "- item $it with | pipes and * stars" }
        body.update(hostile)
        check(body.componentCount <= 32) { "components ${body.componentCount}" }
        // A closing blank line + more text must not multiply them.
        body.update(hostile + "\n\ntail")
        check(body.componentCount <= 32) {
            "components after settle: ${body.componentCount}"
        }
        assertNoHtmlComponents(body)
    }

    // ------------------------------------------------------- turn grouping

    @Test
    fun consecutiveAssistantRowsShareOneTurnAndUserRowsSplitIt() {
        val model = TranscriptModel()
        val ops = ArrayList<TranscriptRowOp>()
        ops += model.upsertDurable(listOf(user(1, 1, "go"))) { false }
        ops += model.upsertDurable(listOf(assistant(2, 2, "part one"))) { false }
        ops += model.upsertDurable(listOf(assistant(3, 3, "part two"))) { false }
        ops += model.upsertDurable(listOf(user(4, 4, "again"))) { false }
        ops += model.upsertDurable(listOf(assistant(5, 5, "second turn"))) { false }
        val byKey = ops.filterIsInstance<TranscriptRowOp.Upsert>().associateBy { it.key.id }
        val first = byKey.getValue("2")
        val second = byKey.getValue("3")
        val third = byKey.getValue("5")
        check(first.payload.kind == TranscriptRowPayload.KIND_ASSISTANT)
        check(first.payload.group == second.payload.group) {
            "consecutive assistant parts share ONE turn group: ${first.payload} vs ${second.payload}"
        }
        check(first.payload.group != third.payload.group) {
            "a user row splits the turn: ${first.payload} vs ${third.payload}"
        }
        check(byKey.getValue("1").payload.kind == TranscriptRowPayload.KIND_USER)
        check(byKey.getValue("1").payload.identity() == "You")
        check(first.payload.identity() == "Faktor")
    }

    @Test
    fun liveTurnIdsSubdivideARunWithoutAUserRow() {
        val model = TranscriptModel()
        // Durable page: one assistant message reserved (empty text; the live
        // stream owns its content). The durable projection carries no turn.
        model.upsertDurable(listOf(assistant(2, 2, ""))) { false }
        // Live frames carry the op id: turn op-1 streams the first message;
        // a SECOND message streams under turn op-2 — no user row between.
        check(model.applyLiveChunk(chunk(2, "text", 1, 1, "part one", turn = "op-1")))
        check(model.applyLiveChunk(chunk(3, "text", 2, 1, "other turn", turn = "op-2")))
        val ops = model.liveOps()
        val payloads = ops.filterIsInstance<TranscriptRowOp.Upsert>()
            .filter { it.payload.kind == TranscriptRowPayload.KIND_ASSISTANT }
            .associateBy { it.key.id }
        val partOne = payloads.getValue("2")
        val otherTurn = payloads.getValue("3")
        check(partOne.payload.group != otherTurn.payload.group) {
            "known turn ids subdivide the run: ${partOne.payload} vs ${otherTurn.payload}"
        }
        check(otherTurn.payload.group != "") { "a live row carries a group" }
    }

    @Test
    fun optimisticUserRowRekeysToItsDurableTwin() {
        val model = TranscriptModel()
        val provisional = model.addProvisional("sub-1", "hello world")
        check(provisional is TranscriptRowOp.Upsert)
        check(provisional!!.payload.identity() == "You")
        val ops = model.upsertDurable(listOf(user(7, 7, "hello world"))) { false }
        val rekey = ops.filterIsInstance<TranscriptRowOp.Rekey>().single()
        check(rekey.key.kind == TranscriptRowKey.KIND_PROVISIONAL)
        check(rekey.newKey.kind == TranscriptRowKey.KIND_DURABLE)
        check(rekey.payload.identity() == "You")
    }

    // ------------------------------------------- view identity/projection

    @Test
    fun plainTextCarriesYouAndFaktorOncePerTurnGroup() {
        val view = TranscriptView()
        view.upsert(
            "m:1", TranscriptRowPayload(
                TranscriptRowPayload.KIND_USER, "do it", 1, "1", "g:m:1"
            ), null
        )
        view.upsert(
            "m:2", TranscriptRowPayload(
                TranscriptRowPayload.KIND_ASSISTANT, "part one", 2, "2", "g:m:2"
            ), null
        )
        view.upsert(
            "m:3", TranscriptRowPayload(
                TranscriptRowPayload.KIND_ASSISTANT, "part two", 3, "3", "g:m:2"
            ), null
        )
        check(view.plainText() == "You: do it\nFaktor: part one\npart two\n") {
            view.plainText()
        }
        // The identity header shows only on the group's FIRST row.
        check(view.cellOf("m:2")!!.identityLabel.isVisible)
        check(!view.cellOf("m:3")!!.identityLabel.isVisible) {
            "the continuation row must not repeat the Faktor header"
        }
        // A new turn re-shows the header.
        view.upsert(
            "m:4", TranscriptRowPayload(
                TranscriptRowPayload.KIND_ASSISTANT, "turn two", 4, "4", "g:m:4"
            ), null
        )
        check(view.plainText().contains("Faktor: turn two")) { view.plainText() }
        check(view.cellOf("m:4")!!.identityLabel.isVisible)
    }

    @Test
    fun keyedRowsInsertAtTheirAnchorAndTrimFromTheOldest() {
        val view = TranscriptView()
        val sink = TranscriptSink(view, null, UiWorkMetrics(), 300, 1_000)
        sink.upsertRow("m:1", payload("first message"), null)
        sink.upsertRow("m:3", payload("third message"), null)
        sink.flush()
        // Out-of-order arrival: the middle row inserts at its ANCHOR.
        sink.upsertRow("m:2", payload("second message"), beforeKey = "m:3")
        sink.flush()
        val text = view.plainText()
        check(text.indexOf("first") < text.indexOf("second")) { text }
        check(text.indexOf("second") < text.indexOf("third")) { text }

        // Oversized arrivals cross the budget: the OLDEST cells trim, the
        // newest suffix survives.
        sink.upsertRow("m:9", payload("x".repeat(200)), null)
        sink.upsertRow("m:10", payload("y".repeat(100)), null)
        sink.flush()
        check(sink.charCount <= 300) { "budget ${sink.charCount}" }
        val retained = view.plainText()
        check(retained.contains("x".repeat(64))) { "newest survived: $retained" }
        check(!retained.contains("first message")) { "oldest trimmed: $retained" }

        // A LATE page re-offers a trimmed row: tombstoned, never re-inserted
        // (content stays exactly-once inside the retained window).
        sink.upsertRow("m:1", payload("first message"), null)
        sink.flush()
        check(view.plainText() == retained) {
            "a trimmed row must never be re-inserted"
        }
    }

    @Test
    fun approvalPartsInterleaveWithTurnPartsByArrival() {
        val view = TranscriptView()
        view.upsert("m:2", payload("working"), null)
        view.appendPart(
            TranscriptRowPayload(
                TranscriptRowPayload.KIND_APPROVAL, "Permission granted", 0, "part-0",
                group = "g:m:2"
            )
        )
        view.upsert("m:3", payload("done"), null)
        val keys = (0 until view.rowCount()).mapNotNull { view.keyAt(it) }
        check(keys == listOf("m:2", "ap:0", "m:3")) { keys }
        val text = view.plainText()
        check(text.indexOf("working") < text.indexOf("Permission granted")) { text }
        check(text.indexOf("Permission granted") < text.indexOf("done")) { text }
    }

    // ------------------------------------------- code blocks and links

    @Test
    fun codeBlocksRenderWithLanguageLabelAndCopyAction() {
        val blocks = RichMarkdown.parse("```kotlin\nval x = 1\nprintln(x)\n```")
        val view = createBlockView(blocks.single(), uiPanelFont())
        val code = view as CodeBlockView
        check(code.languageLabel.text == "kotlin") { code.languageLabel.text }
        check(code.code == "val x = 1\nprintln(x)") { code.code }

        var copied: String? = null
        HostClipboard.recorder = { copied = it }
        try {
            for (listener in code.copyButton.actionListeners) {
                listener.actionPerformed(java.awt.event.ActionEvent(code.copyButton, 0, "copy"))
            }
        } finally {
            HostClipboard.recorder = null
        }
        check(copied == "val x = 1\nprintln(x)") {
            "the Copy action must ride the host clipboard seam: $copied"
        }

        // A fence without a language degrades to the generic "code" label.
        val generic = createBlockView(
            RichMarkdown.parse("```\nplain\n```").single(), uiPanelFont()
        ) as CodeBlockView
        check(generic.languageLabel.text == "code") { generic.languageLabel.text }
    }

    @Test
    fun linkClicksReValidateTheSchemeAtActivationTime() {
        var opened: String? = null
        HostLinks.opener = { opened = it }
        try {
            check(HostLinks.open("https://example.com/a"))
            check(opened == "https://example.com/a") { "opened=$opened" }
            opened = null
            check(!HostLinks.open("javascript:alert(1)"))
            check(opened == null) { "an unsafe scheme must never reach the host: $opened" }
            check(!HostLinks.open("file:///etc/passwd"))
            check(!HostLinks.open("ide://command"))
        } finally {
            HostLinks.opener = null
        }

        // The pane records ONLY safe-scheme ranges.
        val pane = RichTextPane(uiPanelFont())
        pane.apply(
            MarkdownBlock.Paragraph(
                listOf(
                    InlineSpan.Text("see "),
                    InlineSpan.Link("docs", "https://example.com/docs"),
                    InlineSpan.Text(" and "),
                    InlineSpan.Link("evil", "javascript:alert(1)")
                )
            )
        )
        check(pane.links.size == 1) { pane.links }
        check(pane.links.single().second == "https://example.com/docs")
    }

    // ------------------------------------------------------------ helpers

    private fun payload(text: String): TranscriptRowPayload = TranscriptRowPayload(
        TranscriptRowPayload.KIND_ASSISTANT, text, 0, "", "g:t"
    )

    private fun user(seq: Long, id: Long, text: String): NativeMessage =
        NativeMessage(seq, id, "user", 0L, text)

    private fun assistant(seq: Long, id: Long, text: String): NativeMessage =
        NativeMessage(seq, id, "assistant", 0L, text)

    private fun chunk(
        messageId: Long,
        kind: String,
        sequence: Long,
        revision: Long,
        content: String,
        turn: String? = null,
        offset: Long = 0L
    ): LiveChunkFrame = LiveChunkFrame(
        instance = "aaaaaaaaaaaaaaaa",
        session = 7,
        messageId = messageId,
        kind = kind,
        partId = "$messageId:$kind",
        turn = turn,
        attempt = 0,
        sequence = sequence,
        revision = revision,
        offset = offset,
        content = content
    )
}

/** Message-carrying check (the stdlib overload only takes a lazy message). */
private fun check(value: Boolean, message: String) {
    if (!value) throw AssertionError(message)
}

/** Walk a component tree; the transcript must never host an HTML editor. */
private fun assertNoHtmlComponents(root: Container) {
    fun walk(node: Container) {
        for (child in node.components) {
            // NOTE: JTextPane IS-A JEditorPane; the contract is that no
            // component ever uses the HTML editor kit or HTML content.
            val editor = child as? JEditorPane
            check(editor == null || editor.contentType != "text/html") {
                "no HTML editor kit may render transcript text: " +
                    "${editor?.contentType} on ${child.javaClass.name}"
            }
            val text = (child as? javax.swing.text.JTextComponent)?.text ?: ""
            check(!text.startsWith("<html")) { "raw HTML text rendered: ${text.take(60)}" }
            if (child is Container) walk(child)
        }
    }
    walk(root)
}
