// I03 host adapter: the ONLY file that touches the platform editor APIs for
// change review. It implements the pure [ChangeReviewHost] seam by opening
// the served immutable artifact BYTES as a read-only `LightVirtualFile` in
// the editor area (`FileEditorManager`), or a true two-sided diff
// (`DiffManager` + `SimpleDiffRequest`) when two immutable artifacts for the
// same path are served. It never opens a live workspace file: the seam hands
// it bytes and a title, and no method here accepts a file path.
package dev.faktor.frontend

import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.project.Project
import com.intellij.testFramework.LightVirtualFile

class ChangeReviewAdapter(private val project: Project) : ChangeReviewHost {

    override fun openArtifact(artifact: ImmutableArtifact) {
        val file = immutableFile(artifact)
        FileEditorManager.getInstance(project).openFile(file, true)
    }

    override fun openDiff(title: String, left: ImmutableArtifact, right: ImmutableArtifact) {
        val leftFile = immutableFile(left)
        val rightFile = immutableFile(right)
        val factory = DiffContentFactory.getInstance()
        val request = SimpleDiffRequest(
            title,
            factory.create(project, leftFile),
            factory.create(project, rightFile),
            left.title,
            right.title
        )
        DiffManager.getInstance().showDiff(project, request)
    }

    /**
     * One immutable, read-only virtual file: `isWritable = false` makes the
     * opened editor refuse edits and saves, so the immutable basis can never
     * be written back anywhere. The name is the full immutable title.
     */
    private fun immutableFile(artifact: ImmutableArtifact): LightVirtualFile {
        val file = LightVirtualFile(artifact.title, artifact.text)
        file.isWritable = false
        return file
    }
}
