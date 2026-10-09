// Dedicated permissions surface (upstream `session/views/permission`):
// every pending permission request of the session with its capability and
// detail, the selected request's full payload and explicit Allow/Deny
// replies over the daemon's permission reply route. Pure presentation: the
// decision is delivered to a Listener; an unavailable route is recorded
// truthfully (never rendered as "no permissions").
package dev.faktor.frontend

import dev.faktor.shared.NativePermissionEntry
import java.awt.BorderLayout
import javax.swing.BorderFactory
import javax.swing.DefaultListModel
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.ListSelectionModel

class PermissionsPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onPermissionReply(permission: NativePermissionEntry, decision: String)

        fun onRefresh()
    }

    private val model = DefaultListModel<NativePermissionEntry>()

    private val list = JList(model)

    private val detail = compactArea(4, monospace = true)

    private val header = WrappedLabel("pending permissions: -")

    private val allowButton = primaryButton("Allow")

    private val denyButton = secondaryButton("Deny")

    private val refreshButton = secondaryButton("Refresh")

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    /** The last typed reply refusal (409 conflict / session mismatch), if any. */
    private var refusal: String? = null

    init {
        list.selectionMode = ListSelectionModel.SINGLE_SELECTION
        list.addListSelectionListener { applySelection() }
        allowButton.addActionListener { reply("allow") }
        denyButton.addActionListener { reply("deny") }
        refreshButton.addActionListener { listener?.onRefresh() }
        val detailBody = JPanel(BorderLayout(0, Spacing.S))
        detailBody.isOpaque = false
        detailBody.add(JScrollPane(detail), BorderLayout.CENTER)
        detailBody.add(
            actionRow(allowButton, denyButton, refreshButton),
            BorderLayout.SOUTH
        )
        val body = pageColumn()
        body.add(card("Pending requests", JScrollPane(list)))
        body.add(vSpace(Spacing.S))
        body.add(card("Request detail", detailBody))
        header.font = sectionTitleFont()
        val headerRow = JPanel(BorderLayout())
        headerRow.isOpaque = true
        headerRow.background = panelSurface()
        headerRow.border = BorderFactory.createEmptyBorder(
            Spacing.S, Spacing.M, 0, Spacing.M
        )
        headerRow.add(header, BorderLayout.CENTER)
        add(headerRow, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        updateButtons()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the pending list; a selected id is preserved when possible. */
    fun update(permissions: List<NativePermissionEntry>) {
        available = true
        reason = null
        refusal = null
        val selectedId = list.selectedValue?.id
        model.clear()
        for (permission in permissions) model.addElement(permission)
        if (selectedId != null) {
            for (i in 0 until model.size()) {
                if (model.getElementAt(i).id == selectedId) {
                    list.selectedIndex = i
                    break
                }
            }
        } else if (model.size() > 0) {
            list.selectedIndex = 0
        } else {
            detail.text = "no pending permission requests"
        }
        updateButtons()
    }

    /** The route failed (or is absent): record it instead of an empty list. */
    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        refusal = null
        model.clear()
        detail.text = detailText
        updateButtons()
    }

    /**
     * One reply was refused with the daemon's typed 409 (unknown/expired/
     * already resolved, or a waiter owned by another session). The pending
     * list is kept and the refusal is recorded explicitly — never retried
     * behind the operator's back.
     */
    fun setReplyRefusal(detailText: String) {
        refusal = detailText
        detail.text = detailText
        updateButtons()
    }

    /** The last typed reply refusal, or null when the last view was clean. */
    fun refusalText(): String? = refusal

    fun count(): Int = model.size()

    fun available(): Boolean = available

    fun selected(): NativePermissionEntry? = list.selectedValue

    fun unitLabel(index: Int): String {
        val permission = model.getElementAt(index)
        return "#${permission.id} session=${permission.sessionId} " +
            "capability=${permission.capability} detail=${bound(permission.detail, 120)}"
    }

    fun allowEnabled(): Boolean = allowButton.isEnabled

    fun denyEnabled(): Boolean = denyButton.isEnabled

    fun headerText(): String = header.fullText

    fun detailText(): String = detail.text

    /** Programmatic reply, as a click on Allow/Deny would produce. */
    fun submitReply(decision: String) {
        reply(decision)
    }

    private fun reply(decision: String) {
        val permission = list.selectedValue
        if (permission != null) {
            listener?.onPermissionReply(permission, decision)
        }
    }

    private fun applySelection() {
        val permission = list.selectedValue
        if (permission == null) {
            detail.text = if (model.size() == 0) "no pending permission requests" else "select a request"
        } else {
            detail.text = "#${permission.id}\nsession: ${permission.sessionId}" +
                "\ncapability: ${permission.capability}\ndetail: ${bound(permission.detail, 240)}"
        }
        updateButtons()
    }

    private fun updateButtons() {
        val selected = list.selectedValue != null
        allowButton.isEnabled = available && selected
        denyButton.isEnabled = available && selected
        header.text = if (!available) {
            "pending permissions: unavailable ($reason)"
        } else {
            val refused = if (refusal != null) " (last reply refused)" else ""
            "pending permissions: ${model.size()}$refused"
        }
    }
}
