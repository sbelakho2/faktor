// TEST-SEPARATION: oracle=external-fixture isolation=separate-process,read-only-artifacts
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class Edition8ConfigDraftTest {
    private fun draft(): Edition8ConfigDraft {
        val draft = Edition8ConfigDraft()
        draft.load(
            """
            {"revision":7,"runtime_revision":7,"values":{
              "faktor.generation.temperature":{"value":0.2,"locked":true},
              "faktor.budget.warnAtPercent":{"value":80,"locked":false},
              "faktor.permissions.allowedTools":{"value":["read_file"],"locked":false},
              "faktor.browser.enabled":{"value":false,"locked":false}
            }}
            """.trimIndent()
        )
        return draft
    }

    private fun changes(body: String): Map<String, JsonValue> {
        val root = JsonCodec.parse(body) as JsonValue.Obj
        val entries = (root.fields["changes"] as JsonValue.Arr).items
        return entries.associate { entry ->
            val row = entry as JsonValue.Obj
            (row.fields["key"] as JsonValue.Str).value to (row.fields["value"] ?: JsonValue.Null)
        }
    }

    @Test
    fun `staging preserves the descriptor JSON type`() {
        val draft = draft()
        draft.stage("faktor.budget.warnAtPercent", "90")
        draft.stage("faktor.permissions.allowedTools", "read_file, list_directory")
        draft.stage("faktor.browser.enabled", "true")

        val staged = changes(draft.patchBody(7))
        assertEquals(JsonValue.Int64(90), staged["faktor.budget.warnAtPercent"])
        assertEquals(
            JsonValue.Arr(listOf(JsonValue.Str("read_file"), JsonValue.Str("list_directory"))),
            staged["faktor.permissions.allowedTools"],
        )
        assertEquals(JsonValue.Bool(true), staged["faktor.browser.enabled"])
    }

    @Test
    fun `a locked stored-only value never stages a write`() {
        val draft = draft()
        draft.stage("faktor.generation.temperature", "1.4")
        assertTrue(draft.changes().isEmpty())
        assertTrue(draft.patchBody(7).contains("\"changes\":[]"))
    }
}
