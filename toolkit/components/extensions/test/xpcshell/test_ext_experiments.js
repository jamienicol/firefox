"use strict";

/* globals browser */
const { AddonSettings } = ChromeUtils.importESModule(
  "resource://gre/modules/addons/AddonSettings.sys.mjs"
);
const { AddonManager } = ChromeUtils.importESModule(
  "resource://gre/modules/AddonManager.sys.mjs"
);
const { TestUtils } = ChromeUtils.importESModule(
  "resource://testing-common/TestUtils.sys.mjs"
);

AddonTestUtils.init(this);
AddonTestUtils.overrideCertDB();
AddonTestUtils.createAppInfo(
  "xpcshell@tests.mozilla.org",
  "XPCShell",
  "1",
  "42"
);

add_task(async function setup() {
  await ExtensionTestUtils.startAddonManager();
});

let fooExperimentAPIs = {
  foo: {
    schema: "schema.json",
    parent: {
      scopes: ["addon_parent"],
      script: "parent.js",
      paths: [["experiments", "foo", "parent"]],
    },
    child: {
      scopes: ["addon_child"],
      script: "child.js",
      paths: [
        ["experiments", "foo", "child"],
        ["experiments", "foo", "onChildEvent"],
      ],
    },
  },
};

let fooExperimentFiles = {
  "schema.json": JSON.stringify([
    {
      namespace: "experiments.foo",
      types: [
        {
          id: "Meh",
          type: "object",
          properties: {},
        },
      ],
      functions: [
        {
          name: "parent",
          type: "function",
          async: true,
          parameters: [],
        },
        {
          name: "child",
          type: "function",
          parameters: [],
          returns: { type: "string" },
        },
      ],
      events: [
        {
          name: "onChildEvent",
          type: "function",
          parameters: [],
        },
      ],
    },
  ]),

  /* globals ExtensionAPI */
  "parent.js": () => {
    this.foo = class extends ExtensionAPI {
      getAPI() {
        return {
          experiments: {
            foo: {
              parent() {
                return Promise.resolve("parent");
              },
            },
          },
        };
      }
    };
  },

  "child.js": () => {
    this.foo = class extends ExtensionAPI {
      getAPI(context) {
        const EventManagerWithAssertions = class
          extends ExtensionCommon.EventManager
        {
          constructor(...args) {
            super(...args);
            this.assertResetOnIdleOnEvent();
          }

          assertResetOnIdleOnEvent() {
            const expectResetIdleOnEventFalse =
              this.context.extension.persistentBackground;
            if (expectResetIdleOnEventFalse && this.resetIdleOnEvent) {
              const details = {
                eventManagerName: this.name,
                resetIdleOnEvent: this.resetIdleOnEvent,
                envType: this.context.envType,
                viewType: this.context.viewType,
                isBackgroundContext: this.context.isBackgroundContext,
                persistentBackground:
                  this.context.extension.persistentBackground,
              };
              throw new Error(
                `EventManagerWithAssertions: resetIdleOnEvent should be forcefully set to false - ${JSON.stringify(
                  details
                )}`
              );
            }
          }
        };
        return {
          experiments: {
            foo: {
              child() {
                return "child";
              },
              onChildEvent: new EventManagerWithAssertions({
                context,
                name: `experiments.foo.onChildEvent`,
                register: () => {
                  return () => {};
                },
              }).api(),
            },
          },
        };
      }
    };
  },
};

async function testFooExperiment() {
  browser.test.assertEq(
    "object",
    typeof browser.experiments,
    "typeof browser.experiments"
  );

  browser.test.assertEq(
    "object",
    typeof browser.experiments.foo,
    "typeof browser.experiments.foo"
  );

  browser.test.assertEq(
    "function",
    typeof browser.experiments.foo.child,
    "typeof browser.experiments.foo.child"
  );

  browser.test.assertEq(
    "function",
    typeof browser.experiments.foo.parent,
    "typeof browser.experiments.foo.parent"
  );

  browser.test.assertEq(
    "child",
    browser.experiments.foo.child(),
    "foo.child()"
  );

  browser.test.assertEq(
    "parent",
    await browser.experiments.foo.parent(),
    "await foo.parent()"
  );
}

async function testFooFailExperiment() {
  browser.test.assertEq(
    "object",
    typeof browser.experiments,
    "typeof browser.experiments"
  );

  browser.test.assertEq(
    "undefined",
    typeof browser.experiments.foo,
    "typeof browser.experiments.foo"
  );
}

add_task(async function test_bundled_experiments() {
  let testCases = [
    { isSystem: true, temporarilyInstalled: true, shouldHaveExperiments: true },
    {
      isSystem: true,
      temporarilyInstalled: false,
      shouldHaveExperiments: true,
    },
    {
      isPrivileged: true,
      temporarilyInstalled: true,
      shouldHaveExperiments: true,
    },
    {
      isPrivileged: true,
      temporarilyInstalled: false,
      shouldHaveExperiments: true,
    },
    {
      isPrivileged: false,
      temporarilyInstalled: true,
      shouldHaveExperiments: AddonSettings.EXPERIMENTS_ENABLED,
    },
    {
      isPrivileged: false,
      temporarilyInstalled: false,
      shouldHaveExperiments: AppConstants.MOZ_APP_NAME == "thunderbird",
    },
  ];

  async function background(shouldHaveExperiments) {
    if (shouldHaveExperiments) {
      await testFooExperiment();
    } else {
      await testFooFailExperiment();
    }

    browser.test.notifyPass("background.experiments.foo");
  }

  for (let testCase of testCases) {
    let extension = ExtensionTestUtils.loadExtension({
      isPrivileged: testCase.isPrivileged,
      isSystem: testCase.isSystem,
      temporarilyInstalled: testCase.temporarilyInstalled,

      manifest: {
        experiment_apis: fooExperimentAPIs,
      },

      background: `
        ${testFooExperiment}
        ${testFooFailExperiment}
        (${background})(${testCase.shouldHaveExperiments});
      `,

      files: fooExperimentFiles,
    });

    if (testCase.temporarilyInstalled && !testCase.shouldHaveExperiments) {
      ExtensionTestUtils.failOnSchemaWarnings(false);
      await Assert.rejects(
        extension.startup(),
        /Using 'experiment_apis' requires a privileged add-on/,
        "startup failed without experimental api access"
      );
      ExtensionTestUtils.failOnSchemaWarnings(true);
    } else {
      await extension.startup();

      await extension.awaitFinish("background.experiments.foo");

      await extension.unload();
    }
  }
});

add_task(async function test_unbundled_experiments() {
  async function background() {
    await testFooExperiment();

    browser.test.assertEq(
      "object",
      typeof browser.experiments.crunk,
      "typeof browser.experiments.crunk"
    );

    browser.test.assertEq(
      "function",
      typeof browser.experiments.crunk.child,
      "typeof browser.experiments.crunk.child"
    );

    browser.test.assertEq(
      "function",
      typeof browser.experiments.crunk.parent,
      "typeof browser.experiments.crunk.parent"
    );

    browser.test.assertEq(
      "crunk-child",
      browser.experiments.crunk.child(),
      "crunk.child()"
    );

    browser.test.assertEq(
      "crunk-parent",
      await browser.experiments.crunk.parent(),
      "await crunk.parent()"
    );

    browser.test.notifyPass("background.experiments.crunk");
  }

  let extension = ExtensionTestUtils.loadExtension({
    isPrivileged: true,

    manifest: {
      experiment_apis: fooExperimentAPIs,

      permissions: ["experiments.crunk"],
    },

    background: `
      ${testFooExperiment}
      (${background})();
    `,

    files: fooExperimentFiles,
  });

  let apiExtension = ExtensionTestUtils.loadExtension({
    isPrivileged: true,

    manifest: {
      browser_specific_settings: {
        gecko: { id: "crunk@experiments.addons.mozilla.org" },
      },

      experiment_apis: {
        crunk: {
          schema: "schema.json",
          parent: {
            scopes: ["addon_parent"],
            script: "parent.js",
            paths: [["experiments", "crunk", "parent"]],
          },
          child: {
            scopes: ["addon_child"],
            script: "child.js",
            paths: [["experiments", "crunk", "child"]],
          },
        },
      },
    },

    files: {
      "schema.json": JSON.stringify([
        {
          namespace: "experiments.crunk",
          types: [
            {
              id: "Meh",
              type: "object",
              properties: {},
            },
          ],
          functions: [
            {
              name: "parent",
              type: "function",
              async: true,
              parameters: [],
            },
            {
              name: "child",
              type: "function",
              parameters: [],
              returns: { type: "string" },
            },
          ],
        },
      ]),

      "parent.js": () => {
        this.crunk = class extends ExtensionAPI {
          getAPI() {
            return {
              experiments: {
                crunk: {
                  parent() {
                    return Promise.resolve("crunk-parent");
                  },
                },
              },
            };
          }
        };
      },

      "child.js": () => {
        this.crunk = class extends ExtensionAPI {
          getAPI() {
            return {
              experiments: {
                crunk: {
                  child() {
                    return "crunk-child";
                  },
                },
              },
            };
          }
        };
      },
    },
  });

  await apiExtension.startup();
  await extension.startup();

  await extension.awaitFinish("background.experiments.crunk");

  await extension.unload();
  await apiExtension.unload();
});

add_task(async function test_eventpage_with_experiments_resetOnIdleAssert() {
  async function event_page() {
    browser.test.log("EventPage startup");
    // We expect EventManagerWithAssertions instance to throw
    // here if the resetIdleOnEvent didn't got forcefully
    // set to false for the EventManager instantiated in
    // the child process.
    browser.experiments.foo.onChildEvent.addListener(() => {});
    browser.test.sendMessage("eventpage:ready");
  }

  const extension = ExtensionTestUtils.loadExtension({
    isPrivileged: true,
    manifest: {
      experiment_apis: fooExperimentAPIs,
      background: { persistent: false },
    },

    background: event_page,

    files: fooExperimentFiles,
  });

  await extension.startup();

  await extension.awaitMessage("eventpage:ready");

  await extension.unload();
});

// Each script returns the URL it was loaded from.
let scriptURLExperimentAPIs = {
  scriptURL: {
    schema: "schema.json",
    parent: {
      scopes: ["addon_parent"],
      script: "parent.js",
      paths: [["experiments", "scriptURL", "parent"]],
    },
    child: {
      scopes: ["addon_child"],
      script: "child.js",
      paths: [["experiments", "scriptURL", "child"]],
    },
  },
};

let scriptURLExperimentFiles = {
  "schema.json": JSON.stringify([
    {
      namespace: "experiments.scriptURL",
      functions: [
        {
          name: "parent",
          type: "function",
          async: true,
          parameters: [{ name: "value", type: "string", optional: true }],
        },
        {
          name: "child",
          type: "function",
          parameters: [],
          returns: { type: "string" },
        },
      ],
    },
  ]),

  "parent.js": () => {
    this.scriptURL = class extends ExtensionAPI {
      getAPI() {
        return {
          experiments: {
            scriptURL: {
              parent(value) {
                if (value) {
                  Services.obs.notifyObservers(null, "script-url", value);
                }
                return Promise.resolve(new Error().fileName);
              },
            },
          },
        };
      }
    };
  },

  "child.js": () => {
    this.scriptURL = class extends ExtensionAPI {
      getAPI() {
        return {
          experiments: {
            scriptURL: {
              child() {
                return new Error().fileName;
              },
            },
          },
        };
      }
    };
  },
};

add_task(async function test_experiment_script_urls() {
  async function background() {
    browser.test.sendMessage("urls", {
      parent: await browser.experiments.scriptURL.parent(),
      child: browser.experiments.scriptURL.child(),
    });
  }

  let extension = ExtensionTestUtils.loadExtension({
    useAddonManager: "permanent",
    manifest: {
      browser_specific_settings: { gecko: { id: "script-urls@test" } },
      experiment_apis: scriptURLExperimentAPIs,
    },
    background,
    files: scriptURLExperimentFiles,
  });

  await extension.startup();
  let parentURL = extension.extension.getURL("parent.js");
  let childURL = extension.extension.getURL("child.js");

  let urls = await extension.awaitMessage("urls");
  equal(urls.parent, parentURL, "Parent script loaded from moz-extension URL");
  equal(urls.child, childURL, "Child script loaded from moz-extension URL");

  info("Restart so that the experiment modules come from the startup cache");
  await AddonTestUtils.promiseRestartManager();
  await extension.awaitStartup();

  urls = await extension.awaitMessage("urls");
  equal(urls.parent, parentURL, "Parent script URL after restart");
  equal(urls.child, childURL, "Child script URL after restart");

  await extension.unload();
});

add_task(async function test_unpacked_temporary_experiment_without_id() {
  // Allows temporarily installed add-ons to use experiment_apis.
  Services.prefs.setBoolPref("extensions.experiments.enabled", true);
  registerCleanupFunction(() =>
    Services.prefs.clearUserPref("extensions.experiments.enabled")
  );

  let dir = await IOUtils.createUniqueDirectory(
    PathUtils.profileDir,
    "unpacked-experiment"
  );
  let manifest = {
    manifest_version: 2,
    name: "Unpacked experiment without id",
    version: "1",
    background: { scripts: ["background.js"] },
    experiment_apis: scriptURLExperimentAPIs,
  };
  let files = {
    "manifest.json": JSON.stringify(manifest),
    "background.js": `browser.experiments.scriptURL.parent(
      browser.experiments.scriptURL.child()
    );`,
  };
  for (let [name, contents] of Object.entries(scriptURLExperimentFiles)) {
    files[name] = typeof contents == "function" ? `(${contents})();` : contents;
  }
  for (let [name, contents] of Object.entries(files)) {
    await IOUtils.writeUTF8(PathUtils.join(dir, name), contents);
  }

  let reported = TestUtils.topicObserved("script-url");
  let addon = await AddonManager.installTemporaryAddon(new FileUtils.File(dir));
  let [, childURL] = await reported;
  equal(
    childURL,
    WebExtensionPolicy.getByID(addon.id).getURL("child.js"),
    "Child script of unpacked add-on loaded from moz-extension URL"
  );

  await addon.uninstall();
  await IOUtils.remove(dir, { recursive: true });
});
