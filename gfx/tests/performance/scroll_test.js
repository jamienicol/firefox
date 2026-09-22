/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
/* global ChromeTask, SimpleTest, add_task, info, ok, prepareScrollTest */

"use strict";

function mean(values) {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function startFrameTimeRecording() {
  return ChromeTask.spawn(null, () => {
    const chromeWindow = Services.wm.getMostRecentWindow(null);
    return chromeWindow.windowUtils.startFrameTimeRecording();
  });
}

function stopFrameTimeRecording(handle) {
  return ChromeTask.spawn(handle, recordingHandle => {
    const chromeWindow = Services.wm.getMostRecentWindow(null);
    return chromeWindow.windowUtils.stopFrameTimeRecording(recordingHandle);
  });
}

add_task(async function measureScrollPerformance() {
  SimpleTest.requestCompleteLog();
  if (typeof prepareScrollTest === "function") {
    await prepareScrollTest();
  }

  const scrollingElement = document.scrollingElement;
  const bottom = scrollingElement.scrollHeight - innerHeight;
  ok(bottom > 0, "The test page is scrollable");
  ok(Math.abs(scrollingElement.scrollTop) < 1, "The test starts at the top");

  const scrollEnd = new Promise(resolve => {
    window.addEventListener("scrollend", resolve, { once: true });
  });
  scrollingElement.style.scrollBehavior = "smooth";

  const handle = await startFrameTimeRecording();
  window.scrollTo(0, bottom);
  await scrollEnd;
  const intervals = await stopFrameTimeRecording(handle);

  ok(intervals.length > 0, "Recorded presented frame intervals");
  info(
    "perfMetrics",
    JSON.stringify({
      mean_presented_frame_interval: mean(intervals),
    })
  );
});
