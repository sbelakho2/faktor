// Native model picker (S02): a bounded modal over the served catalog with
// exact capability details. Favorites/recents are user-local; availability
// and capability authority always come from the served catalog/capabilities,
// never from a marketing name.
package dev.faktor.frontend

import dev.faktor.shared.NativeModelInfo
import java.awt.Dialog
import java.awt.Window
import javax.swing.JDialog
import javax.swing.KeyStroke

internal object ModelPickerDialog {

    /** Shows the modal picker and returns the selected served model, or null. */
    fun choose(
        owner: java.awt.Component?,
        models: List<NativeModelInfo>,
        capabilities: Map<String, Map<String, String>> = emptyMap()
    ): NativeModelInfo? {
        var selected: NativeModelInfo? = null
        val window: Window? = owner?.let { javax.swing.SwingUtilities.getWindowAncestor(it) }
        val dialog = JDialog(
            window,
            "Choose model",
            Dialog.ModalityType.APPLICATION_MODAL
        )
        val panel = Edition5ModelPickerPanel(
            catalog = models,
            capabilities = capabilities,
            onSelect = {
                selected = it
                dialog.dispose()
            },
            onCancel = { dialog.dispose() }
        )
        dialog.contentPane = panel
        dialog.defaultCloseOperation = JDialog.DISPOSE_ON_CLOSE
        dialog.rootPane.registerKeyboardAction(
            { dialog.dispose() },
            KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ESCAPE, 0),
            javax.swing.JComponent.WHEN_IN_FOCUSED_WINDOW
        )
        dialog.pack()
        dialog.setSize(
            Math.max(560, dialog.width.coerceAtMost(880)),
            Math.max(420, dialog.height.coerceAtMost(640))
        )
        dialog.setLocationRelativeTo(owner)
        dialog.isVisible = true
        return selected
    }
}
