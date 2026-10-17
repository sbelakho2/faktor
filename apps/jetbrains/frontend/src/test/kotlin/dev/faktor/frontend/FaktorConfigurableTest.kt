// I08 platform test: the APPLICATION-level `FaktorConfigurable` is
// registered where the platform actually loads it, its display name comes
// from the class, `apply()` persists through the shared controller exactly
// once and maps invalid input onto ConfigurationException without a partial
// write, and `reset()` only reloads the persisted state (the credential
// store is never touched: that path is covered by the pure smoke with a fake
// PasswordSafe). Runs headless under `:frontend:test`.
package dev.faktor.frontend

import com.intellij.openapi.options.Configurable
import com.intellij.openapi.options.ConfigurationException
import com.intellij.testFramework.fixtures.BasePlatformTestCase

class FaktorConfigurableTest : BasePlatformTestCase() {

    override fun tearDown() {
        try {
            val properties = com.intellij.ide.util.PropertiesComponent.getInstance()
            for (key in listOf(
                FaktorSettingKeys.BINARY_PATH,
                FaktorSettingKeys.RUN_PROVIDER,
                FaktorSettingKeys.RUN_MODEL,
                FaktorSettingKeys.RUN_MUTATION_MODE,
                FaktorSettingKeys.CONTROL_PLANE_ENDPOINT,
                FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION,
                FaktorSettingKeys.CONTROL_PLANE_SESSION
            )) {
                properties.unsetValue(key)
            }
        } finally {
            super.tearDown()
        }
    }

    fun testApplicationConfigurableEpResolvesToTheClass() {
        val configurable = FaktorConfigurable()
        assertEquals("Faktor", configurable.displayName)
        val ep = Configurable.APPLICATION_CONFIGURABLE.extensionList
            .firstOrNull { it.id == "dev.faktor.settings" }
        assertNotNull("the applicationConfigurable EP must be registered", ep)
        val created = ep!!.createConfigurable()!!
        assertTrue(
            "the EP must instantiate FaktorConfigurable, got ${created.javaClass.name}",
            created is FaktorConfigurable
        )
        assertEquals("Faktor", created.displayName)
        // The EP carries no project scope: application-level registration.
        assertEquals("dev.faktor.settings", ep.id)
    }

    fun testLegacyProjectControlPlaneCoordinatesMigrateToTheSharedApplicationStore() {
        val projectProperties = com.intellij.ide.util.PropertiesComponent.getInstance(project)
        projectProperties.setValue(
            FaktorSettingKeys.CONTROL_PLANE_ENDPOINT,
            "http://127.0.0.1:9"
        )
        projectProperties.setValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION, "org-legacy")
        projectProperties.setValue(FaktorSettingKeys.CONTROL_PLANE_SESSION, "sess-legacy")
        val store = IntelliJControlPlaneScopeStore()
        store.migrateFromProject(project)
        val application = com.intellij.ide.util.PropertiesComponent.getInstance()
        assertEquals(
            "http://127.0.0.1:9",
            application.getValue(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT)
        )
        assertEquals(
            "org-legacy",
            application.getValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION)
        )
        assertEquals(
            "sess-legacy",
            application.getValue(FaktorSettingKeys.CONTROL_PLANE_SESSION)
        )
        // The legacy per-project keys are removed by the migration (exactly
        // once); a stale project value can never resurrect after the
        // application value is cleared.
        for (key in listOf(
            FaktorSettingKeys.CONTROL_PLANE_ENDPOINT,
            FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION,
            FaktorSettingKeys.CONTROL_PLANE_SESSION
        )) {
            assertNull(
                "legacy project key $key must be removed by the migration",
                projectProperties.getValue(key)
            )
        }
        // Re-running the migration is a strict no-op.
        store.migrateFromProject(project)
        assertEquals(
            "http://127.0.0.1:9",
            application.getValue(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT)
        )
        // An explicit application value wins over a legacy project copy (and
        // the stale copy is still removed).
        application.setValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION, "org-app")
        projectProperties.setValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION, "org-stale")
        store.migrateFromProject(project)
        assertEquals(
            "org-app",
            application.getValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION)
        )
        assertNull(
            "a stale project copy must not survive an application-scoped value",
            projectProperties.getValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION)
        )
    }

    fun testApplyPersistsValidValuesExactlyOnce() {
        val configurable = FaktorConfigurable()
        configurable.setFieldsForTest(
            binaryPath = null,
            provider = "alpha",
            model = "alpha-1",
            mutationMode = "shadow",
            controlPlaneEndpoint = "http://127.0.0.1:9",
            controlPlaneOrganization = "org-local"
        )
        assertTrue("edited fields must report modified", configurable.isModified())
        configurable.apply()
        val properties = com.intellij.ide.util.PropertiesComponent.getInstance()
        assertEquals("alpha", properties.getValue(FaktorSettingKeys.RUN_PROVIDER))
        assertEquals("alpha-1", properties.getValue(FaktorSettingKeys.RUN_MODEL))
        assertEquals("shadow", properties.getValue(FaktorSettingKeys.RUN_MUTATION_MODE))
        assertEquals(
            "http://127.0.0.1:9",
            properties.getValue(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT)
        )
        assertEquals("org-local", properties.getValue(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION))
        // The applied state is the new baseline: no phantom edit remains.
        assertFalse("apply must clear the modified flag", configurable.isModified())
    }

    fun testInvalidInputRaisesConfigurationExceptionAndWritesNothing() {
        val configurable = FaktorConfigurable()
        configurable.setFieldsForTest(
            binaryPath = null,
            provider = "alpha",
            model = "alpha-1",
            mutationMode = "default",
            controlPlaneEndpoint = "http://127.0.0.1:9",
            controlPlaneOrganization = "org-local"
        )
        configurable.apply()
        val properties = com.intellij.ide.util.PropertiesComponent.getInstance()
        val before = listOf(
            FaktorSettingKeys.RUN_PROVIDER,
            FaktorSettingKeys.RUN_MODEL,
            FaktorSettingKeys.RUN_MUTATION_MODE,
            FaktorSettingKeys.CONTROL_PLANE_ENDPOINT,
            FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION
        ).associateWith { properties.getValue(it) }
        // An unknown mutation mode is refused with a useful message and no
        // write at all (the previous configuration stays exact).
        configurable.setFieldsForTest(
            binaryPath = null,
            provider = "beta",
            model = "beta-1",
            mutationMode = "direct",
            controlPlaneEndpoint = "http://127.0.0.1:9",
            controlPlaneOrganization = "org-local"
        )
        try {
            configurable.apply()
            fail("an unknown mutation mode must raise ConfigurationException")
        } catch (e: ConfigurationException) {
            val text = e.toString()
            assertTrue(
                "the message must name the mutation mode: $text",
                text.contains("mutation mode")
            )
        }
        for ((key, value) in before) {
            assertEquals("no partial write for $key", value, properties.getValue(key))
        }
    }

    fun testResetReloadsThePersistedConfigurationWithoutTouchingTheCredential() {
        val properties = com.intellij.ide.util.PropertiesComponent.getInstance()
        properties.setValue(FaktorSettingKeys.RUN_PROVIDER, "alpha")
        properties.setValue(FaktorSettingKeys.RUN_MUTATION_MODE, "shadow")
        val configurable = FaktorConfigurable()
        configurable.setFieldsForTest(
            binaryPath = null,
            provider = "edited",
            model = "edited-1",
            mutationMode = "default",
            controlPlaneEndpoint = "",
            controlPlaneOrganization = ""
        )
        assertTrue(configurable.isModified())
        configurable.reset()
        assertFalse("reset must discard the edits", configurable.isModified())
        assertEquals("alpha", configurable.editedConfigurationForTest().provider)
        assertEquals("shadow", configurable.editedConfigurationForTest().mutationMode)
        assertEquals("", configurable.editedConfigurationForTest().controlPlaneEndpoint)
    }

    fun testRestoreDefaultsPutsDefaultsIntoTheUiWithoutWriting() {
        val properties = com.intellij.ide.util.PropertiesComponent.getInstance()
        properties.setValue(FaktorSettingKeys.RUN_PROVIDER, "alpha")
        properties.setValue(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT, "http://127.0.0.1:9")
        val configurable = FaktorConfigurable()
        configurable.restoreDefaults()
        val defaults = configurable.editedConfigurationForTest()
        assertEquals(FaktorSettingsSnapshot.DEFAULTS, defaults)
        assertEquals("", defaults.controlPlaneEndpoint)
        // Restoring defaults is a UI action: the persisted store is untouched
        // (Apply persists; sign-out is the only credential deletion path).
        assertEquals("alpha", properties.getValue(FaktorSettingKeys.RUN_PROVIDER))
        assertEquals(
            "http://127.0.0.1:9",
            properties.getValue(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT)
        )
    }
}
