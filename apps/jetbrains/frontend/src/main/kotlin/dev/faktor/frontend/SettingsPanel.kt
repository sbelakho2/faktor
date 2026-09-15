// Settings + provider-selection surface (upstream `settings/**` and the
// session model picker): the registered providers and their known models
// from `GET /native/providers` (falling back to the `GET /models` catalog
// when the registry route is absent), the daemon identity the session runs
// against, and the mutation-mode value the Task composer submits.
// The mutation surface is shadow-only: `default`/`shadow`, never the
// removed direct-owner mode (which cannot be selected or read back).
// Selection is held here and read by the panel when it creates a session or
// starts a task; nothing is fabricated when a route is unavailable.
package dev.faktor.frontend

import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.NativeProviderInfo
import java.awt.BorderLayout
import java.awt.FlowLayout
import java.awt.GridLayout
import javax.swing.DefaultComboBoxModel
import javax.swing.JButton
import javax.swing.JComboBox
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import java.awt.event.ActionListener

class SettingsPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onRefreshProviders()

        fun onProviderSelectionChanged(provider: String, model: String)
    }

    private val providerModel = DefaultComboBoxModel<String>()

    private val providerCombo = JComboBox(providerModel)

    private val modelModel = DefaultComboBoxModel<String>()

    private val modelCombo = JComboBox(modelModel)

    private val mutationModel = DefaultComboBoxModel<String>()

    private val mutationCombo = JComboBox(mutationModel)

    private val providerArea = compactArea(6)

    private val daemonArea = compactArea(5)

    private val status = JLabel("providers: -")

    private val refreshButton = JButton("Refresh providers")

    private var listener: Listener? = null

    private var providers: List<NativeProviderInfo> = emptyList()

    private var modelsByProvider: Map<String, List<String>> = emptyMap()

    private var available = true

    private var reason: String? = null

    private var suppressEvents = false

    init {
        mutationModel.addElement("default")
        mutationModel.addElement("shadow")
        providerCombo.addActionListener(ActionListener { providerChanged() })
        modelCombo.addActionListener(ActionListener { emitSelection() })
        refreshButton.addActionListener { listener?.onRefreshProviders() }
        val selectors = JPanel(GridLayout(2, 2, 4, 4))
        selectors.add(JLabel("provider"))
        selectors.add(JLabel("model"))
        selectors.add(providerCombo)
        selectors.add(modelCombo)
        val actions = JPanel(FlowLayout(FlowLayout.LEFT, 4, 0))
        actions.add(refreshButton)
        val body = JPanel(GridLayout(0, 1, 0, 4))
        body.add(titledSection("provider selection", selectors))
        body.add(titledSection("mutation mode (Task composer)", mutationCombo))
        body.add(titledSection("providers", JScrollPane(providerArea)))
        body.add(titledSection("daemon", JScrollPane(daemonArea)))
        body.add(titledSection("actions", actions))
        add(status, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        daemonArea.text = "daemon: not started"
        providerArea.text = "no providers served"
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** The registry view (`GET /native/providers`). */
    fun setProviders(list: List<NativeProviderInfo>) {
        available = true
        reason = null
        providers = list
        val byProvider = LinkedHashMap<String, List<String>>()
        for (provider in list) {
            byProvider[provider.instanceId] = provider.models.map { it.model }
        }
        modelsByProvider = byProvider
        val selected = providerCombo.selectedItem as? String
        suppressEvents = true
        providerModel.removeAllElements()
        for (provider in list) providerModel.addElement(provider.instanceId)
        if (selected != null && list.any { it.instanceId == selected }) {
            providerCombo.selectedItem = selected
        } else if (list.isNotEmpty()) {
            providerCombo.selectedIndex = 0
        }
        suppressEvents = false
        refreshModelCombo()
        val text = StringBuilder()
        for (provider in list) {
            text.append(provider.instanceId).append(" [").append(provider.family).append("] ")
            text.append(provider.healthStatus)
            text.append(" models=").append(provider.models.size)
            if (provider.runtimeContextLimitSupported) text.append(" runtime-context-limit")
            text.append('\n')
            for (model in provider.models) {
                text.append("  ").append(model.model)
                text.append(" ctx=").append(model.context)
                text.append(" tools=").append(model.tools)
                text.append(" reasoning=").append(model.reasoning)
                text.append(" source=").append(model.source)
                text.append('\n')
            }
        }
        providerArea.text = if (text.isEmpty()) "no providers served" else text.toString()
        updateStatus()
    }

    /** Catalog fallback (`GET /models`) when no registry route is served. */
    fun setCatalog(list: List<NativeModelInfo>) {
        available = true
        reason = null
        val byProvider = LinkedHashMap<String, MutableList<String>>()
        for (model in list) {
            var models = byProvider[model.provider]
            if (models == null) {
                models = ArrayList<String>()
                byProvider[model.provider] = models
            }
            models.add(model.model)
        }
        val frozen = LinkedHashMap<String, List<String>>()
        for ((provider, models) in byProvider) frozen[provider] = models
        modelsByProvider = frozen
        val selected = providerCombo.selectedItem as? String
        suppressEvents = true
        providerModel.removeAllElements()
        for (provider in frozen.keys) providerModel.addElement(provider)
        if (selected != null && frozen.containsKey(selected)) {
            providerCombo.selectedItem = selected
        } else if (frozen.isNotEmpty()) {
            providerCombo.selectedIndex = 0
        }
        suppressEvents = false
        refreshModelCombo()
        val text = StringBuilder()
        for (model in list) {
            text.append(model.provider).append('/').append(model.model)
            text.append(" ctx=").append(model.context)
            text.append(" tools=").append(model.tools)
            text.append(" reasoning=").append(model.reasoning)
            text.append(" source=").append(model.source)
            text.append('\n')
        }
        providerArea.text = if (text.isEmpty()) "no models served" else text.toString()
        updateStatus()
    }

    fun setDaemonInfo(binaryPath: String, dataDir: String, version: String, baseUrl: String) {
        daemonArea.text = "binary: $binaryPath\ndata dir: $dataDir\n" +
            "version: $version\nbase url: $baseUrl"
    }

    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        providers = emptyList()
        modelsByProvider = emptyMap()
        suppressEvents = true
        providerModel.removeAllElements()
        modelModel.removeAllElements()
        suppressEvents = false
        providerArea.text = detailText
        updateStatus()
    }

    fun selectedProvider(): String? = providerCombo.selectedItem as? String

    fun selectedModel(): String? = modelCombo.selectedItem as? String

    fun mutationMode(): String? {
        val selected = mutationCombo.selectedItem as? String ?: return null
        return selected.takeIf { it != "default" && mutationModes().contains(it) }
    }

    fun available(): Boolean = available

    fun providerCount(): Int = providerModel.size

    fun providerLabel(index: Int): String {
        if (index < providers.size) {
            val provider = providers[index]
            return "${provider.instanceId} [${provider.family}] ${provider.healthStatus}" +
                " models=${provider.models.size}"
        }
        val name = providerModel.getElementAt(index)
        val models = modelsByProvider[name] ?: emptyList()
        return "$name models=${models.size}"
    }

    fun modelCount(): Int = modelModel.size

    fun modelLabel(index: Int): String = modelModel.getElementAt(index)

    fun selectProvider(name: String) {
        providerCombo.selectedItem = name
    }

    fun selectModel(name: String) {
        modelCombo.selectedItem = name
    }

    /** Select one offered mode; an unknown/removed mode is refused (no-op). */
    fun selectMutationMode(mode: String) {
        val index = (0 until mutationModel.size)
            .indexOfFirst { mutationModel.getElementAt(it) == mode }
        if (index >= 0) mutationCombo.selectedIndex = index
    }

    fun mutationModes(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until mutationModel.size) out.add(mutationModel.getElementAt(i))
        return out
    }

    fun statusText(): String = status.text

    fun providerText(): String = providerArea.text

    fun daemonText(): String = daemonArea.text

    private fun providerChanged() {
        refreshModelCombo()
        emitSelection()
    }

    private fun refreshModelCombo() {
        val provider = providerCombo.selectedItem as? String
        val models = if (provider == null) emptyList() else modelsByProvider[provider] ?: emptyList()
        val selected = modelCombo.selectedItem as? String
        suppressEvents = true
        modelModel.removeAllElements()
        for (model in models) modelModel.addElement(model)
        if (selected != null && models.contains(selected)) {
            modelCombo.selectedItem = selected
        } else if (models.isNotEmpty()) {
            modelCombo.selectedIndex = 0
        }
        suppressEvents = false
        updateStatus()
    }

    private fun emitSelection() {
        if (suppressEvents) return
        val provider = providerCombo.selectedItem as? String
        val model = modelCombo.selectedItem as? String
        if (provider != null && model != null) {
            listener?.onProviderSelectionChanged(provider, model)
        }
    }

    private fun updateStatus() {
        status.text = if (!available) {
            "providers: unavailable ($reason)"
        } else {
            "providers: ${providerModel.size} provider(s), " +
                "selected=${providerCombo.selectedItem ?: "-"}/${modelCombo.selectedItem ?: "-"}"
        }
    }
}
