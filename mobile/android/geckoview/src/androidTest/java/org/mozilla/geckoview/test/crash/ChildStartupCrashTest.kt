package org.mozilla.geckoview.test.crash

import android.content.Context
import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.filters.MediumTest
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.hamcrest.Matchers.equalTo
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeThat
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.mozilla.geckoview.BuildConfig
import org.mozilla.geckoview.GeckoRuntime
import org.mozilla.geckoview.test.BaseSessionTest
import org.mozilla.geckoview.test.TestCrashHandler
import org.mozilla.geckoview.test.TestRuntimeService
import org.mozilla.geckoview.test.rule.GeckoSessionTestRule.ClosedSessionAtStart

@RunWith(AndroidJUnit4::class)
@MediumTest
class ChildStartupCrashTest : BaseSessionTest() {
    private val targetContext
        get() = InstrumentationRegistry.getInstrumentation().targetContext

    private val timeout
        get() = sessionRule.env.defaultTimeoutMillis

    private class Instance(
        context: Context,
        profileFolder: File,
        private val env: List<String>,
    ) :
        TestRuntimeService.RuntimeInstance<RuntimeCrashTestService>(
            context,
            RuntimeCrashTestService::class.java,
            profileFolder,
        ) {
        override fun createIntent(context: Context): Intent =
            super.createIntent(context).apply {
                env.forEachIndexed { i, value -> putExtra("env$i", value) }
            }

        fun start(): Instance = apply { sendIntent() }
    }

    @Test
    @ClosedSessionAtStart
    fun crashContentBeforeGeckoLoaded() {
        assumeTrue(BuildConfig.MOZ_CRASHREPORTER)

        // RuntimeCrashTestService doesn't enable the app zygote.
        assumeThat(sessionRule.env.isAppZygoteProcess, equalTo(false))

        val client = TestCrashHandler.Client(targetContext)
        assertTrue(client.connect(timeout))
        client.setEvalNextCrashDump(
            GeckoRuntime.CRASHED_PROCESS_VISIBILITY_FOREGROUND_CHILD,
            "content",
            null,
            "Simulated child process startup crash",
        )

        val env = mutableListOf("MOZ_ANDROID_TEST_CHILD_STARTUP_CRASH=tab")
        if (sessionRule.env.isIsolatedProcess) {
            env.add("MOZ_ANDROID_CONTENT_SERVICE_ISOLATED_PROCESS=1")
        }
        val runtime = Instance(targetContext, temporaryProfile.get(), env).start()
        try {
            runtime.loadTestPath(HELLO_HTML_PATH)

            val evalResult = client.getEvalResult(timeout)
            assertTrue(evalResult.mMsg, evalResult.mResult)
        } finally {
            client.disconnect()
            if (runtime.isConnected) {
                runCatching { sessionRule.waitForResult(runtime.quit()) }
            }
        }
    }
}
