// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
//
// JUnit entry points for the live-output + turn-reducer contract. Each test
// delegates to the executable smoke's scenario (the same assertions the
// `:frontend:smoke` lane runs), so `:frontend:test` and the host smoke can
// never drift. The fixtures are the fake live/transcript daemons defined in
// NativeLiveStreamSmoke.kt (external process/socket), never the SUT grading
// itself.
package dev.faktor.frontend

import org.junit.Test

class NativeLiveStreamTest {

    @Test
    fun reducerStateMatrix() = NativeLiveStreamSmoke.reducerStateMatrix()

    @Test
    fun utf8ByteOffsetsNeverMixWithUtf16() =
        NativeLiveStreamSmoke.utf8ByteOffsetsNeverMixWithUtf16()

    @Test
    fun liveFramesAdvanceBeforeDurableFlush() =
        NativeLiveStreamSmoke.liveFramesAdvanceBeforeDurableFlush()

    @Test
    fun duplicateOmitReplayResyncIsByteIdentical() =
        NativeLiveStreamSmoke.duplicateOmitReplayResyncIsByteIdentical()

    @Test
    fun foreignInstanceFramesAreDropped() =
        NativeLiveStreamSmoke.foreignInstanceFramesAreDropped()

    @Test
    fun durableWinsDivergenceAndKeepsNewerLiveSuffix() =
        NativeLiveStreamSmoke.durableWinsDivergenceAndKeepsNewerLiveSuffix()

    @Test
    fun reconnectReconcilesAuthoritativeSnapshot() =
        NativeLiveStreamSmoke.reconnectReconcilesAuthoritativeSnapshot()

    @Test
    fun stopTargetsAcceptedOperation() = NativeLiveStreamSmoke.stopTargetsAcceptedOperation()

    @Test
    fun queuedFollowUpRendersQueued() = NativeLiveStreamSmoke.queuedFollowUpRendersQueued()

    @Test
    fun orderedToolRowSitsBetweenTextRows() =
        NativeLiveStreamSmoke.orderedToolRowSitsBetweenTextRows()

    @Test
    fun assistantMarkdownStaysPlainStructuralText() =
        NativeLiveStreamSmoke.assistantMarkdownStaysPlainStructuralText()

    @Test
    fun streamClientIsSnapshotFirstAndResyncs() =
        NativeLiveStreamSmoke.streamClientIsSnapshotFirstAndResyncs()

    @Test
    fun streamClientNonRetryableRefusalIsUnavailable() =
        NativeLiveStreamSmoke.streamClientNonRetryableRefusalIsUnavailable()

    @Test
    fun subscribeBeforeSubmitOpensTheLiveHandshakeFirst() =
        NativeLiveStreamSmoke.subscribeBeforeSubmitOpensTheLiveHandshakeFirst()
}
