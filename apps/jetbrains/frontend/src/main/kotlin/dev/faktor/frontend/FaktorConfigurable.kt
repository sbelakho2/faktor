// I08 — the platform Configurable for APPLICATION-level Faktor settings.
//
// Registered in plugin.xml as `<applicationConfigurable instance=.../>` (the
// registration form the bundled 2024.1 platform actually supports). The
// daemon binary path is application-level and must NEVER be
// project-configurable, so this Configurable is application-scoped by
// construction; no project/workspace can contribute any value here.
//
// The Configurable is a thin view over the shared pure
// [FaktorSettingsController]: `isModified` compares the edited fields to the
// applied snapshot, `apply()` validates and persists atomically through one
// batch write (a [FaktorSettingsValidationException] becomes the platform's
// ConfigurationException and writes nothing), and `reset()` restores the
// defaults without touching the IDE credential store. Control-plane
// credentials are read/written ONLY through PasswordSafe; the token is never
// a field here and never enters any settings value.
package dev.faktor.frontend

import com.intellij.ide.util.PropertiesComponent
import com.intellij.openapi.fileChooser.FileChooser
import com.intellij.openapi.options.Configurable
import com.intellij.openapi.options.ConfigurationException
import com.intellij.openapi.progress.ProgressManager
import dev.faktor.backend.DaemonBinary
import java.awt.BorderLayout
import java.nio.file.Files
import java.nio.file.Paths
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JTextField
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

/**
 * The IntelliJ application properties store as the shared [FaktorSettingsStore]
 * seam: the ONLY writer path for application-level Faktor settings. One
 * [write] call carries the full validated batch.
 */
class IntelliJApplicationSettingsStore : FaktorSettingsStore {

    private val properties: PropertiesComponent
        get() = PropertiesComponent.getInstance()

    override fun read(key: String): String? = properties.getValue(key)

    override fun write(values: Map<String, String?>) {
        for ((key, value) in values) {
            if (value == null) {
                properties.unsetValue(key)
            } else {
                properties.setValue(key, value)
            }
        }
    }
}

class FaktorConfigurable : Configurable {

    private val controller = FaktorSettingsController(IntelliJApplicationSettingsStore())

    private val secretVault: SecretVault = IntelliJPasswordSafeVault()

    private val credentialStore = ControlPlaneCredentialStore(secretVault)

    private val sessionStore = IntelliJControlPlaneSessionStore()

    private var component: JComponent? = null

    private var binaryField: JTextField? = null

    private var providerField: JTextField? = null

    private var modelField: JTextField? = null

    private var mutationCombo: JComboBox<String>? = null

    private var endpointField: JTextField? = null

    private var organizationField: JTextField? = null

    private var trustLabel: JLabel? = null

    private var signedInLabel: JLabel? = null

    private var applied: FaktorSettingsSnapshot = FaktorSettingsSnapshot.DEFAULTS

    /** The Edition 5 configuration center embedded under the Faktor node. */
    private var edition5Panel: Edition5SettingsPanel? = null

    /** The shared staging/validation controller (one per Configurable). */
    private var edition5Controller: Edition5SettingsController? = null

    override fun getDisplayName(): String = "Faktor"

    override fun createComponent(): JComponent {
        val existing = component
        if (existing != null) return existing
        val binary = JTextField(28)
        val provider = JTextField(20)
        val model = JTextField(20)
        val mutation = JComboBox(FaktorSettingsRules.MUTATION_MODES.toTypedArray())
        val endpoint = JTextField(28)
        val organization = JTextField(20)
        val trust = JLabel("—")
        val signedIn = JLabel("")
        binaryField = binary
        providerField = provider
        modelField = model
        mutationCombo = mutation
        endpointField = endpoint
        organizationField = organization
        trustLabel = trust
        signedInLabel = signedIn

        fun fieldChanged() {
            refreshTrust()
            refreshSignedIn()
        }
        val listener = object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent?) = fieldChanged()
            override fun removeUpdate(e: DocumentEvent?) = fieldChanged()
            override fun changedUpdate(e: DocumentEvent?) = fieldChanged()
        }
        binary.document.addDocumentListener(listener)
        endpoint.document.addDocumentListener(listener)
        organization.document.addDocumentListener(listener)

        val locateButton = secondaryButton("Locate daemon…")
        locateButton.toolTipText =
            "Pick the faktor-cli executable; the path is stored at application scope on Apply"
        locateButton.addActionListener {
            val chosen = FileChooser.chooseFile(FaktorDaemonSettings.daemonChooserDescriptor(), null, null)
            if (chosen != null) {
                binary.text = chosen.path
            }
        }
        val restoreService = secondaryButton("Restore default")
        restoreService.addActionListener {
            binary.text = ""
            refreshTrust()
        }
        val restoreRun = secondaryButton("Restore default")
        restoreRun.addActionListener {
            provider.text = ""
            model.text = ""
            mutation.selectedItem = FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE
        }
        val restoreControlPlane = secondaryButton("Restore default")
        restoreControlPlane.addActionListener {
            endpoint.text = ""
            organization.text = ""
            refreshSignedIn()
        }
        // Local deletion only: this Configurable has no daemon client, so it
        // must not label itself a full sign-out. The tool window's sign-out is
        // the revoking path (it deletes the local row too).
        val signOutButton = secondaryButton("Remove stored credential")
        signOutButton.toolTipText =
            "Delete the PasswordSafe row for this endpoint + organization. The tool window's " +
                "Sign out also revokes the remote session."
        signOutButton.addActionListener { signOutControlPlane() }

        val serviceBody = pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(formRow("Daemon binary path", binary))
            add(actionRow(locateButton, restoreService))
            add(row("Trust status", trust))
            add(
                wrappedMutedLabel(
                    "Application scope only: no project or workspace can set this path. A binary " +
                        "without a checksum record always requires the exact-path start confirmation."
                )
            )
        }
        val runBody = pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(formRow("Provider", provider))
            add(formRow("Model", model))
            add(formRow("Mutation mode", mutation))
            add(actionRow(restoreRun))
            add(
                wrappedMutedLabel(
                    "Applied to the next session the tool window creates. Shadow-only: the " +
                        "removed direct-owner mode cannot be selected."
                )
            )
        }
        val controlPlaneBody = pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(formRow("Endpoint", endpoint))
            add(formRow("Organization", organization))
            add(actionRow(restoreControlPlane, signOutButton))
            add(row("State", signedIn))
            add(
                wrappedMutedLabel(
                    "Optional. The credential itself lives only in the IDE credential store " +
                        "(PasswordSafe), keyed by endpoint + organization; it is never rendered " +
                        "here and never written to settings."
                )
            )
        }
        val screensButton = secondaryButton("Open Revision 8 screens…")
        screensButton.toolTipText = "Open the 72 Revision 8 Work, Inspect and History screens"
        screensButton.a11y(
            "Open Revision 8 screens",
            "Opens the registry-driven screen browser for providers, context, tools, usage and setup."
        )
        screensButton.addActionListener {
            Edition8ScreenBrowser(edition8Host()).show(component)
        }
        val screensBody = pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(actionRow(screensButton))
            add(
                wrappedMutedLabel(
                    "The screens are the host presentation of the same native contracts used by the " +
                        "tool window. Secrets stay in PasswordSafe; no screen fabricates a Saved, " +
                        "Connected or Verified state."
                )
            )
        }
        val body = pageColumn().apply {
            add(card("Service", serviceBody))
            add(vSpace(Spacing.M))
            add(card("Run defaults", runBody))
            add(vSpace(Spacing.M))
            add(card("Optional control plane", controlPlaneBody))
            add(vSpace(Spacing.M))
            add(card("Revision 8 screens", screensBody))
        }
        // The Edition 5 settings tree: General (the host-local application
        // settings above) plus every daemon-backed category. The same panel
        // renders in the tool window Config utility and the offscreen matrix.
        val source: Edition5ConfigSource =
            FaktorConnectionRegistry.client()?.let { NativeEdition5ConfigSource(it) }
                ?: Edition5ConfigUnavailable(
                    "contract-required: daemon not connected; start the Faktor service to read " +
                        "GET ${Edition5Routes.CONFIG_EFFECTIVE} (revision + per-field effective snapshot)"
                )
        val controller = Edition5SettingsController(source, endpointLabel = FaktorConnectionRegistry.endpointLabel())
        edition5Controller = controller
        val general = object : Edition5SettingsPanel.GeneralSettingsSurface {
            override val component: JComponent get() = body
            override fun generalState(): String =
                "Application scope only. Run defaults apply to the next session; the binary path " +
                    "and optional control-plane coordinates never enter a project."
            override fun applyGeneral(): String? = null
        }
        val center = Edition5SettingsPanel(controller, general)
        edition5Panel = center
        component = center.component()
        reset()
        return center.component()
    }

    private fun edition8Host(): Edition8Host = Edition8Host(
        client = { FaktorConnectionRegistry.client() },
        sessionId = { null },
        taskId = { null },
        vault = secretVault,
        settings = controller,
        hostAction = { id -> "host action $id acknowledged; the Settings host owns this preference" },
        navigate = { screenId ->
            javax.swing.SwingUtilities.invokeLater {
                Edition8ScreenBrowser(edition8Host()).show(component, screenId)
            }
        },
        credentials = credentialStore
    )

    private fun formRow(label: String, field: JComponent): JPanel {
        val row = JPanel(BorderLayout(Spacing.S, 0))
        row.isOpaque = false
        row.add(mutedLabel(label), BorderLayout.WEST)
        row.add(field, BorderLayout.CENTER)
        return row
    }

    private fun row(label: String, value: JComponent): JPanel {
        val panel = pageColumn(gap = Spacing.XS, padding = 0)
        panel.add(mutedLabel(label))
        panel.add(value)
        return panel
    }

    /** True when any edited field differs from the applied configuration. */
    override fun isModified(): Boolean =
        editedSnapshot() != applied || (edition5Panel?.changedCount() ?: 0) > 0

    /** Validate first, then persist in ONE batch; invalid writes nothing. */
    override fun apply() {
        val snapshot = editedSnapshot()
        try {
            controller.apply(snapshot)
        } catch (e: FaktorSettingsValidationException) {
            throw ConfigurationException(e.message ?: "invalid Faktor configuration")
        }
        // Daemon-backed staged changes are applied as ONE atomic patch. The
        // IntelliJ progress runner executes the I/O on a background thread;
        // the EDT only receives the typed acknowledgement or refusal.
        val center = edition5Panel
        if (center != null && center.changedCount() > 0) {
            var decision: Edition5ConfigDecision? = null
            ProgressManager.getInstance().runProcessWithProgressSynchronously(
                { decision = center.applyBlocking() },
                "Applying Faktor configuration",
                false,
                null
            )
            val result = decision
                ?: throw ConfigurationException("Faktor configuration apply produced no decision")
            if (!result.ok) {
                throw ConfigurationException(
                    result.contractGap
                        ?: result.errors.joinToString("; ") { error ->
                            (error.key?.plus(": ") ?: "") + error.message
                        }.ifBlank { "the daemon refused the configuration patch" }
                )
            }
        }
        applied = controller.read()
        render(applied)
        refreshTrust()
        refreshSignedIn()
    }

    /**
     * Restore the defaults in the UI. The credential store is never touched:
     * a stored control-plane credential survives a reset (sign-out is the
     * only deletion path).
     */
    override fun reset() {
        applied = controller.read()
        render(applied)
        edition5Panel?.reset()
    }

    /**
     * The visible `Restore default` action: put the DEFAULTS into the UI
     * (Apply persists them) without touching the store or the credential.
     */
    fun restoreDefaults() {
        createComponent()
        render(FaktorSettingsSnapshot.DEFAULTS)
    }

    override fun disposeUIResources() {
        edition5Panel?.shutdown()
        edition5Panel = null
        edition5Controller = null
        component = null
        binaryField = null
        providerField = null
        modelField = null
        mutationCombo = null
        endpointField = null
        organizationField = null
        trustLabel = null
        signedInLabel = null
    }

    /** The edited configuration exactly as the fields carry it. */
    private fun editedSnapshot(): FaktorSettingsSnapshot = FaktorSettingsSnapshot(
        binaryPath = FaktorSettingsRules.storedOrNull(binaryField?.text),
        provider = FaktorSettingsRules.storedOrNull(providerField?.text),
        model = FaktorSettingsRules.storedOrNull(modelField?.text),
        mutationMode = mutationCombo?.selectedItem as? String
            ?: FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE,
        controlPlaneEndpoint = endpointField?.text?.trim().orEmpty(),
        controlPlaneOrganization = organizationField?.text?.trim().orEmpty()
    )

    private fun render(snapshot: FaktorSettingsSnapshot) {
        binaryField?.text = snapshot.binaryPath ?: ""
        providerField?.text = snapshot.provider ?: ""
        modelField?.text = snapshot.model ?: ""
        mutationCombo?.selectedItem = snapshot.mutationMode
        endpointField?.text = snapshot.controlPlaneEndpoint
        organizationField?.text = snapshot.controlPlaneOrganization
        refreshTrust()
        refreshSignedIn()
    }

    /** The D10 trust wording of the CURRENT field value (never guessed). */
    private fun refreshTrust() {
        val label = trustLabel ?: return
        val path = FaktorSettingsRules.storedOrNull(binaryField?.text)
        if (path == null) {
            label.text = "—"
            return
        }
        val parsed = try {
            Paths.get(path)
        } catch (e: Exception) {
            label.text = "—"
            return
        }
        label.text = if (!Files.isRegularFile(parsed)) {
            "—"
        } else {
            daemonTrustLabel(DaemonBinary.checksumTrust(parsed))
        }
    }

    /** Signed-in state from PasswordSafe only; the token itself never renders. */
    private fun refreshSignedIn() {
        val label = signedInLabel ?: return
        val scope = controlPlaneScopeOf()
        label.text = if (scope != null && credentialStore.resolve(scope) != null) {
            "credential stored in the IDE credential store"
        } else {
            "no credential stored"
        }
    }

    private fun controlPlaneScopeOf(): ControlPlaneScope? {
        val endpoint = endpointField?.text?.trim().orEmpty()
        val organization = organizationField?.text?.trim().orEmpty()
        if (endpoint.isEmpty() || organization.isEmpty()) return null
        val scope = ControlPlaneScope(endpoint, organization)
        return scope.takeIf { it.valid() }
    }

    /**
     * Deletes the credential row and the dead session coordinate; the
     * endpoint/organization settings values are untouched. Remote revocation
     * belongs to the tool window's sign-out (the only path with a client).
     */
    private fun signOutControlPlane() {
        val scope = controlPlaneScopeOf() ?: return
        credentialStore.delete(scope)
        sessionStore.write(null)
        refreshSignedIn()
    }

    // ---------------------------------------------------------- test seams

    /**
     * Platform-test seam: the edited configuration exactly as [apply] reads
     * it (the Settings dialog drives the same fields).
     */
    internal fun editedConfigurationForTest(): FaktorSettingsSnapshot {
        createComponent()
        return editedSnapshot()
    }

    /**
     * Platform-test seam: set every field as a user edit would (no
     * persistence, no validation: that is precisely [apply]'s job).
     */
    internal fun setFieldsForTest(
        binaryPath: String?,
        provider: String?,
        model: String?,
        mutationMode: String,
        controlPlaneEndpoint: String,
        controlPlaneOrganization: String
    ) {
        createComponent()
        binaryField?.text = binaryPath ?: ""
        providerField?.text = provider ?: ""
        modelField?.text = model ?: ""
        // A hostile settings import can carry a mode the offered list does
        // not contain; the seam must reproduce that so apply()'s validation
        // is what refuses it.
        mutationCombo?.let { combo ->
            if ((0 until combo.itemCount).none { combo.getItemAt(it) == mutationMode }) {
                combo.addItem(mutationMode)
            }
            combo.selectedItem = mutationMode
        }
        endpointField?.text = controlPlaneEndpoint
        organizationField?.text = controlPlaneOrganization
    }
}
