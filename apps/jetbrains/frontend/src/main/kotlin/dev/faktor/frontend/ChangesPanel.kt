// Changes: the files this run touched, with the evidence refs that back
// them attached inline (contextual detail, not a destination). Rows are
// workspace-relative paths; long paths wrap instead of clipping. Selecting
// an evidence ref routes through the same listener as the tree and opens
// the evidence detail inside the Verification view.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import javax.swing.BoxLayout
import javax.swing.JButton
import javax.swing.JPanel

class ChangesPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onEvidenceSelected(ref: EvidenceRef)
    }

    private val header = WrappedLabel("changes: -")

    private val filesColumn = ScrollableColumn()

    private val evidenceColumn = ScrollableColumn()

    private var listener: Listener? = null

    private var fileCount = 0

    init {
        filesColumn.layout = BoxLayout(filesColumn, BoxLayout.Y_AXIS)
        evidenceColumn.layout = BoxLayout(evidenceColumn, BoxLayout.Y_AXIS)
        val body = pageColumn()
        body.add(card("Changed files", filesColumn))
        body.add(vSpace(Spacing.M))
        body.add(card("Evidence for these changes", evidenceColumn))
        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        update(emptyList(), emptyList())
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the view with the served file list and its evidence refs. */
    fun update(files: List<String>, evidence: List<EvidenceRef>) {
        fileCount = files.size
        filesColumn.removeAll()
        if (files.isEmpty()) {
            filesColumn.add(mutedLabel("No files changed yet. Edits appear here as the run works."))
        } else {
            for (path in files) {
                filesColumn.add(vSpace(Spacing.XS))
                val row = WrappedLabel("• $path")
                row.font = monospacePanelFont()
                row.foreground = textForeground()
                row.alignmentX = Component.LEFT_ALIGNMENT
                filesColumn.add(row)
            }
        }
        evidenceColumn.removeAll()
        if (evidence.isEmpty()) {
            evidenceColumn.add(mutedLabel("No evidence is attached to these changes yet."))
        } else {
            for (ref in evidence) {
                evidenceColumn.add(vSpace(Spacing.XS))
                val button = secondaryButton(ref.label.ifBlank { "Evidence" })
                button.addActionListener { listener?.onEvidenceSelected(ref) }
                val row = JPanel(BorderLayout())
                row.isOpaque = false
                row.alignmentX = Component.LEFT_ALIGNMENT
                row.add(button, BorderLayout.WEST)
                evidenceColumn.add(row)
            }
        }
        filesColumn.revalidate()
        evidenceColumn.revalidate()
        header.text = "changes: " + plural(fileCount, "file")
        revalidate()
        repaint()
    }

    fun fileCount(): Int = fileCount

    fun filesText(): String = buildString {
        for (index in 0 until filesColumn.componentCount) {
            val component = filesColumn.getComponent(index)
            if (component is WrappedLabel) append(component.fullText).append('\n')
        }
    }

    fun evidenceCount(): Int = evidenceColumn.components.count { it is JPanel }
}
