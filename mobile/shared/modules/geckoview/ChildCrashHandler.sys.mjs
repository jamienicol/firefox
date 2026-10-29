/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this file,
 * You can obtain one at http://mozilla.org/MPL/2.0/. */

import { GeckoViewUtils } from "resource://gre/modules/GeckoViewUtils.sys.mjs";
import { AppConstants } from "resource://gre/modules/AppConstants.sys.mjs";

const lazy = {};

ChromeUtils.defineESModuleGetters(lazy, {
  EventDispatcher: "resource://gre/modules/Messaging.sys.mjs",
});

const { debug, warn } = GeckoViewUtils.initLogging("ChildCrashHandler");

function getDir(name) {
  const uAppDataPath = Services.dirsvc.get("UAppData", Ci.nsIFile).path;
  return PathUtils.join(uAppDataPath, "Crash Reports", name);
}

var getPendingDir = function () {
  return getDir("pending");
};

var getPendingMinidump = function (id) {
  const pendingDir = getPendingDir();

  return [".dmp", ".extra"].map(suffix => {
    return PathUtils.join(pendingDir, `${id}${suffix}`);
  });
};

export var crashPullCallback = function (matches, requestedByDevs) {
  lazy.EventDispatcher.instance.sendRequest(
    "GeckoView:RemoteSettingsCrashPull",
    {
      crashIDs: matches.map(id => PathUtils.join(getPendingDir(), `${id}.dmp`)),
    }
  );
};

export var ChildCrashHandler = {
  // Map a child ID to a remote type.
  childMap: new Map(),

  // The event listener for this is hooked up in GeckoViewStartup.sys.mjs
  observe(aSubject, aTopic, aData) {
    const childID = aData;

    switch (aTopic) {
      case "process-type-set":
      // Intentional fall-through
      case "ipc:content-created": {
        const pp = aSubject.QueryInterface(Ci.nsIDOMProcessParent);
        this.childMap.set(childID, pp.remoteType);
        break;
      }

      case "ipc:content-shutdown":
        this.childMap.delete(childID);
        break;

      case "ipc:child-crash-recorded": {
        aSubject.QueryInterface(Ci.nsIPropertyBag2);

        const disableReporting = Services.env.get(
          "MOZ_CRASHREPORTER_NO_REPORT"
        );

        if (!AppConstants.MOZ_CRASHREPORTER || disableReporting) {
          return;
        }

        // If dumpID is empty the process was likely killed by the system and
        // we therefore do not want to report the crash. This includes most
        // "expected" extensions process crashes on Android.
        const dumpID = aSubject.get("dumpID");
        if (!dumpID) {
          return;
        }

        debug`Notifying child process crash, dump ID ${dumpID}`;
        const [minidumpPath, extrasPath] = getPendingMinidump(dumpID);

        const processType = aSubject.get("processType");
        let remoteType;
        if (processType === "content") {
          remoteType = this.childMap.get(childID);
          if (remoteType?.length) {
            // Only send the remote type prefix since everything after a "="
            // is dynamic, and used to control the process pool to use.
            remoteType = remoteType.split("=")[0];
          }
        }

        // Only report crashes of non-extension content processes as
        // occurring in a foreground process, and all others as background.
        const processVisibility =
          processType === "content" && remoteType !== "extension"
            ? "FOREGROUND_CHILD"
            : "BACKGROUND_CHILD";

        lazy.EventDispatcher.instance.sendRequest(
          "GeckoView:ChildCrashReport",
          {
            minidumpPath,
            extrasPath,
            success: true,
            fatal: false,
            processVisibility,
            processType,
            remoteType,
          }
        );

        break;
      }
    }
  },
};
