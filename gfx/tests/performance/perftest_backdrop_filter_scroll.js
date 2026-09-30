/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

/* global require, module, __dirname */
"use strict";

const TEST_PAGE = "backdrop_filter_scroll.html";
const CHECK_MS = 250;

async function test(context, commands) {
  const fs = require("fs");
  const path = require("path");

  const html = fs.readFileSync(path.join(__dirname, TEST_PAGE), "utf8");

  await commands.measure.start("backdrop-filter-scroll");
  await commands.js.run(`
    document.open();
    document.write(${JSON.stringify(html)});
    document.close();
  `);
  await commands.wait.byTime(2000);

  // Use a slow, underdamped smooth scroll animation. It reaches the bottom of
  // the page while still moving quickly, at which point the animation ends,
  // rather than slowly settling at the destination. These are float prefs,
  // which must be set as strings, which browsertime's --firefox.preference
  // cannot do.
  await commands.js.runPrivileged(`
    Services.prefs.setCharPref("layout.css.scroll-snap.spring-constant", "1");
    Services.prefs.setCharPref("layout.css.scroll-snap.damping-ratio", "0.5");
  `);

  // Warm up with an unmeasured scroll.
  await commands.js.run(`
    return new Promise(resolve => {
      window.addEventListener("scrollend", () => resolve(), { once: true });
      window.scrollTo({ top: window.scrollMaxY, behavior: "smooth" });
    });
  `);
  await commands.js.run(`window.scrollTo({ top: 0, behavior: "instant" });`);
  await commands.wait.byTime(1000);

  const scrollable = await commands.js.run(`return window.scrollMaxY > 0;`);
  if (!scrollable) {
    throw new Error("Test page is not tall enough to scroll");
  }

  const startIndex = await commands.js.runPrivileged(
    `return window.windowUtils.startFrameTimeRecording();`
  );
  // Also measure how many device pixels the page moved at the end of the
  // scroll, as seen by the main thread.
  const endDevPixels = await commands.js.run(`
    return new Promise(resolve => {
      const positions = [];
      const onScroll = () => positions.push([performance.now(), window.scrollY]);
      window.addEventListener("scroll", onScroll);
      window.addEventListener("scrollend", () => {
        window.removeEventListener("scroll", onScroll);
        const checkStart = performance.now() - ${CHECK_MS};
        let checkStartY = 0;
        for (const [time, y] of positions) {
          if (time <= checkStart) {
            checkStartY = y;
          }
        }
        resolve((window.scrollY - checkStartY) * window.devicePixelRatio);
      }, { once: true });
      window.scrollTo({ top: window.scrollMaxY, behavior: "smooth" });
    });
  `);
  const recorded = await commands.js.runPrivileged(
    `return window.windowUtils.stopFrameTimeRecording(${startIndex});`
  );
  if (!recorded.length) {
    throw new Error("Frame time recording buffer overflowed");
  }

  // Remove two frames on each side of the recording, as tscrollx does.
  const intervals = recorded.slice(2, -2);
  if (!intervals.length) {
    throw new Error("No frames were recorded");
  }

  // If the content moves by less than a device pixel per frame, frames no
  // longer reflect the cost of rendering. Ensure the scroll was still moving
  // quickly when it ended.
  let endFrames = 0;
  for (let i = intervals.length - 1, t = 0; i >= 0 && t < CHECK_MS; i--) {
    t += intervals[i];
    endFrames++;
  }
  if (endDevPixels < 2 * endFrames) {
    throw new Error(
      `Recorded ${endFrames} frames while moving ${endDevPixels} device pixels`
    );
  }

  const mean = intervals.reduce((a, b) => a + b, 0) / intervals.length;
  context.log.info(
    `Recorded ${intervals.length} frames, mean interval ${mean.toFixed(2)}ms`
  );

  await commands.measure.stop();
  commands.measure.addObject({ frameIntervalMean: mean });
}

module.exports = {
  test,
  owner: "Graphics Team",
  name: "backdrop-filter-scroll",
  description:
    "Measures frame intervals while smooth scrolling a page using backdrop-filter.",
  longDescription: `
  Loads a page with a fixed header and many small elements using
  backdrop-filter, then performs a single smooth scroll to the bottom using an
  underdamped animation, so that it does not slow down at the end. Frame
  intervals are recorded in the compositor for the whole scroll using
  nsIDOMWindowUtils.startFrameTimeRecording/stopFrameTimeRecording, similarly
  to the APZ part of tscrollx. Rendering runs in ASAP mode (layout.frame_rate=0,
  no EGL swap interval), so results are not limited by the display refresh
  rate.
  `,
  supportedBrowsers: ["Firefox"],
  supportedPlatforms: ["Windows", "Linux", "macOS", "Android"],
  options: {
    default: {
      perfherder: true,
      perfherder_transformer: "SingleJsonMedianRetriever",
      perfherder_metrics: [
        { name: "frameIntervalMean", unit: "ms", lowerIsBetter: true },
      ],
      console_metrics: [{ name: "frameIntervalMean" }],
      browsertime_extra_options:
        "firefox.preference=layout.frame_rate:0," +
        "firefox.preference=layout.css.scroll-behavior.same-physics-as-user-input:false," +
        "firefox.preference=gfx.swap-interval.egl:false," +
        "firefox.preference=toolkit.framesRecording.bufferSize:10000",
    },
  },
};
