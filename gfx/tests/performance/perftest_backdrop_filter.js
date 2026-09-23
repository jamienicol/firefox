async function test(context, commands) {
  const url =
    context.options.browsertime && context.options.browsertime.url;
  if (!url) {
    throw new Error("The fixture URL was not configured by the perftest hook");
  }
  await commands.navigate(url);
  await commands.wait.byTime(500);

  const ready = await commands.js.run(
    "return document.documentElement.dataset.fixtureReady === 'true'"
  );
  if (!ready) {
    throw new Error("Backdrop filter fixture did not finish building");
  }

  await commands.js.run("window.scrollTo(0, 0)");
  await commands.wait.byTime(500);
  await commands.measure.start();

  const handle = await commands.js.runPrivileged(`
    const win =
      Services.wm.getMostRecentWindow("navigator:browser") ||
      Services.wm.getMostRecentWindow("navigator:geckoview");
    if (!win) {
      throw new Error("Could not find a browser window");
    }
    return win.windowUtils.startFrameTimeRecording();
  `);

  try {
    await commands.js.run(
      "window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'smooth' })"
    );

    let reachedBottom = false;
    for (let attempt = 0; attempt < 150; attempt++) {
      reachedBottom = await commands.js.run(
        "return window.scrollY + window.innerHeight >= document.documentElement.scrollHeight - 1"
      );
      if (reachedBottom) {
        break;
      }
      await commands.wait.byTime(100);
    }

    if (!reachedBottom) {
      throw new Error("Smooth scroll did not reach the bottom within 15 seconds");
    }
    await commands.wait.byTime(250);
  } finally {
    const intervals = await commands.js.runPrivileged(`
      const win =
        Services.wm.getMostRecentWindow("navigator:browser") ||
        Services.wm.getMostRecentWindow("navigator:geckoview");
      if (!win) {
        throw new Error("Could not find a browser window");
      }
      return win.windowUtils.stopFrameTimeRecording(${handle});
    `);

    await commands.measure.stop();

    const samples = intervals.slice(1);
    if (!samples.length) {
      throw new Error("Frame time recording returned no usable samples");
    }

    const mean = samples.reduce((sum, value) => sum + value, 0) / samples.length;
    await commands.measure.addObject({ frameInterval: mean });
  }
}

module.exports = {
  test,
  owner: "Graphics Team",
  name: "Backdrop filter scrolling",
  description: "Measures frame intervals while scrolling a page with backdrop filters",
  supportedBrowsers: ["firefox"],
  supportedPlatforms: ["desktop", "android"],
  options: {
    default: {
      iterations: 10,
      hooks: "gfx/tests/performance/hooks_backdrop_filter.py",
    },
  },
};
