// Revision 8 IDE-editor host implementation.
//
// This file is part of the PLUGIN build only (it imports the IntelliJ
// platform). The split-mode kotlinc smoke compiles `FaktorChatPanel` without
// platform jars, so the panel consumes the `Edition8HostEditor` seam and this
// class supplies the real editor behaviour in `FaktorToolWindowFactory`.
package dev.faktor.frontend

import com.intellij.ide.DataManager
import com.intellij.openapi.actionSystem.IdeActions
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.editor.actionSystem.EditorActionManager
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.TextRange
import com.intellij.openapi.wm.ToolWindowManager

internal class IntelliJHostEditor(private val project: Project) : Edition8HostEditor {

    private fun editor(): Editor? =
        FileEditorManager.getInstance(project).selectedTextEditor

    override fun inlineCodeAction(): String {
        val editor = editor()
            ?: return "contract-required: no active editor to host an inline code action"
        val selection = editor.selectionModel
        val start = if (selection.hasSelection()) selection.selectionStart else editor.caretModel.offset
        val end = if (selection.hasSelection()) selection.selectionEnd else editor.caretModel.offset
        val version = editor.document.modificationStamp
        val handler = EditorActionManager.getInstance()
            .getActionHandler(IdeActions.ACTION_SHOW_INTENTION_ACTIONS)
        handler.execute(editor, DataManager.getInstance().getDataContext(editor.component))
        return "captured ${editor.virtualFile?.url ?: "untitled"} offset=$start..$end version=$version; intentions opened for a bounded task draft"
    }

    override fun captureSelection(): String? {
        val editor = editor() ?: return null
        val selection = editor.selectionModel
        val text = if (selection.hasSelection()) {
            editor.document.getText(TextRange(selection.selectionStart, selection.selectionEnd))
        } else {
            ""
        }
        return "selection:" + (editor.virtualFile?.url ?: "untitled") + ":" + text.take(2048)
    }

    override fun insertSlashCommand(): String {
        val editor = editor()
            ?: return "No active editor exists; no command text was inserted."
        editor.document.insertString(editor.caretModel.offset, "/")
        return "Inserted the slash-command prefix at the caret; the host palette owns command selection."
    }

    override fun openProblems(): String {
        val tool = ToolWindowManager.getInstance(project).getToolWindow("Problems")
        tool?.activate(null)
        return if (tool == null) {
            "No Problems tool window is available in this IDE."
        } else {
            "Opened the host Problems tool window; diagnostics are host-owned."
        }
    }
}
