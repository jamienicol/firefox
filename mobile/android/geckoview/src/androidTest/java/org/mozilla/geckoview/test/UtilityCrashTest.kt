package org.mozilla.geckoview.test

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.filters.MediumTest
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.mozilla.geckoview.BuildConfig
import org.mozilla.geckoview.GeckoRuntime
import org.mozilla.geckoview.test.rule.GeckoSessionTestRule.IgnoreCrash

@RunWith(AndroidJUnit4::class)
@MediumTest
class UtilityCrashTest : BaseSessionTest() {
    val client = TestCrashHandler.Client(InstrumentationRegistry.getInstrumentation().targetContext)

    @Before
    fun setup() {
        assertTrue(client.connect(sessionRule.env.defaultTimeoutMillis))
        client.setEvalNextCrashDump(
            GeckoRuntime.CRASHED_PROCESS_VISIBILITY_BACKGROUND_CHILD,
            "utility",
            null,
            false,
        )
    }

    @IgnoreCrash
    @Test
    fun crashUtility() {
        // We need the crash reporter for this test
        assumeTrue(BuildConfig.MOZ_CRASHREPORTER)

        sessionRule.crashUtilityProcess()

        val evalResult = client.getEvalResult(sessionRule.env.defaultTimeoutMillis)
        assertTrue(evalResult.mMsg, evalResult.mResult)
    }

    @After
    fun teardown() {
        client.disconnect()
    }
}
