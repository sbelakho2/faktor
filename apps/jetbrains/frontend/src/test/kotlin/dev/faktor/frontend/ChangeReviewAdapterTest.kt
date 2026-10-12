// I03 platform adapter test: the ONE file that touches the platform editor
// APIs opens the served immutable BYTES as a read-only `LightVirtualFile`
// named by the immutable title — never a live workspace file. Runs headless
// under `:frontend:test`.
package dev.faktor.frontend

import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.testFramework.LightVirtualFile
import com.intellij.testFramework.fixtures.BasePlatformTestCase

class ChangeReviewAdapterTest : BasePlatformTestCase() {

    fun testOpenArtifactOpensReadOnlyImmutableView() {
        val adapter = ChangeReviewAdapter(project)
        val artifact = ImmutableArtifact(
            title = "candidate tm1:cand — source diff — evidence 41 (immutable)",
            text = TRUNCATED_BY_POLICY_HEADER + "\ncaf\u00e9 \u2014 served bytes",
            truncatedByPolicy = true,
            evidenceId = 41L
        )
        adapter.openArtifact(artifact)
        val editors = FileEditorManager.getInstance(project).selectedEditors
        assertTrue("the immutable artifact must open an editor", editors.isNotEmpty())
        val file = editors[0].file
        assertTrue("the artifact must be a LightVirtualFile, got ${file.javaClass.name}", file is LightVirtualFile)
        assertEquals(artifact.title, file.name)
        assertFalse("the immutable artifact must be read-only", file.isWritable)
        assertEquals(artifact.text, String(file.contentsToByteArray(), Charsets.UTF_8))
    }
}
