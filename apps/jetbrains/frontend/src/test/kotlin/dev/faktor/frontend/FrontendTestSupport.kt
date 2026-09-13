// Minimal assertion helpers for the frontend test sources. They live in the
// frontend test package (not shared with the backend test sources) so the
// module compiles standalone under Gradle; the kotlinc smoke compiles them
// together with the backend helpers in their own packages.
package dev.faktor.frontend

fun assertEquals(expected: Any?, actual: Any?, message: String? = null) {
    if (expected != actual) {
        throw AssertionError(
            (if (message == null) "" else "$message: ") + "expected <$expected>, actual <$actual>"
        )
    }
}

fun assertTrue(condition: Boolean, message: String? = null) {
    if (!condition) {
        throw AssertionError(message ?: "expected true")
    }
}

fun fail(message: String): Nothing = throw AssertionError(message)
