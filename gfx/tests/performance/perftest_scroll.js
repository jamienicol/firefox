/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
/* global module */

"use strict";

function mean(values) {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

async function measureScroll(context, commands, destination) {
  const handle = await commands.js.runPrivileged(`
    const chromeWindow = Services.wm.getMostRecentWindow(null);
    return chromeWindow.windowUtils.startFrameTimeRecording();
  `);

  await context.selenium.driver.executeAsyncScript(
    `const done = arguments[arguments.length - 1];
     window.addEventListener("scrollend", () => done(), { once: true });
     document.scrollingElement.style.scrollBehavior = "smooth";
     window.scrollTo(0, arguments[0]);`,
    destination
  );

  const intervals = await commands.js.runPrivileged(`
    const chromeWindow = Services.wm.getMostRecentWindow(null);
    return chromeWindow.windowUtils.stopFrameTimeRecording(${handle});
  `);
  if (intervals.length === 0) {
    throw new Error("No presented frame intervals were recorded");
  }
  return mean(intervals);
}

async function test(context, commands) {
  const { page, url } = context.options.browsertime;
  if (!page || !url) {
    throw new Error(
      "The scroll test requires browsertime.page and browsertime.url"
    );
  }
  await commands.navigate(url);

  const pageMetrics = await commands.js.run(`
    return {
      scrollTop: document.scrollingElement.scrollTop,
      scrollHeight: document.scrollingElement.scrollHeight,
      viewportHeight: innerHeight,
    };
  `);
  const bottom = pageMetrics.scrollHeight - pageMetrics.viewportHeight;
  if (bottom <= 0 || Math.abs(pageMetrics.scrollTop) >= 1) {
    throw new Error(
      `The test page is not ready: ${JSON.stringify(pageMetrics)}`
    );
  }

  await commands.measure.start(page);
  const result = await measureScroll(context, commands, bottom);
  await commands.measure.stop();

  context.log.info(`mean_presented_frame_interval: ${result}`);
  await commands.measure.addObject({
    custom_data: { mean_presented_frame_interval: result },
  });
}

module.exports = {
  test,
  owner: "Graphics Team",
  name: "Scroll",
  description:
    "Measure presented frame intervals while smoothly scrolling a page.",
  supportedBrowsers: ["Firefox", "Geckoview_example"],
  supportedPlatforms: ["Desktop", "Android"],
  tags: ["gfx", "scrolling"],
  options: {
    default: {
      hooks: "gfx/tests/performance/hooks_scroll.py",
      browsertime_iterations: 10,
      browser_prefs: {
        "apz.paint_skipping.enabled": false,
        "docshell.event_starvation_delay_hint": 1,
        "dom.send_after_paint_to_content": true,
        "layout.css.scroll-behavior.same-physics-as-user-input": false,
        "layout.css.scroll-snap.spring-constant": "10",
        "layout.frame_rate": 0,
        "toolkit.framesRecording.bufferSize": 10000,
      },
      perfherder: true,
      perfherder_metrics: [
        {
          name: "mean_presented_frame_interval",
          unit: "ms",
          shouldAlert: true,
          lowerIsBetter: true,
        },
      ],
    },
  },
};
