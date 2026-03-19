/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

package org.mozilla.fenix;

import android.app.ActivityManager;
import android.app.Application;
import android.content.Context;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.os.Build;
import android.os.Process;

import androidx.work.Configuration.Builder;
import androidx.work.Configuration.Provider;

import java.lang.reflect.InvocationTargetException;
import java.util.List;

public class BaseApplication extends Application implements Provider {
    private static final String APP_SPLIT_NAME = "app";
    private static final String IMPL_CLASS_NAME = "org.mozilla.fenix.FenixApplication";

    public interface Impl {
        void setApplication(BaseApplication application);

        default Context attachBaseContext(Context base) {
            return base;
        }

        default void onCreate() {}

        default void onTrimMemory(int level) {}

        default void onLowMemory() {}

        default void onConfigurationChanged(Configuration newConfig) {}

        default void onTerminate() {}
    }

    private Impl impl;

    @Override
    protected void attachBaseContext(Context base) {
        Impl loadedImpl = loadImpl(base);
        if (loadedImpl != null) {
            loadedImpl.setApplication(this);
            base = loadedImpl.attachBaseContext(base);
        }
        super.attachBaseContext(base);
        impl = loadedImpl;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        if (impl != null) {
            impl.onCreate();
        }
    }

    @Override
    public void onTrimMemory(int level) {
        super.onTrimMemory(level);
        if (impl != null) {
            impl.onTrimMemory(level);
        }
    }

    @Override
    public void onLowMemory() {
        super.onLowMemory();
        if (impl != null) {
            impl.onLowMemory();
        }
    }

    @Override
    public void onConfigurationChanged(Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
        if (impl != null) {
            impl.onConfigurationChanged(newConfig);
        }
    }

    @Override
    public void onTerminate() {
        if (impl != null) {
            impl.onTerminate();
        }
        super.onTerminate();
    }

    @Override
    public androidx.work.Configuration getWorkManagerConfiguration() {
        if (impl instanceof Provider) {
            return ((Provider) impl).getWorkManagerConfiguration();
        }
        return new Builder().build();
    }

    public Impl getImpl() {
        return impl;
    }

    public Impl requireImpl() {
        if (impl == null) {
            throw new IllegalStateException("Fenix application implementation is not available in this process");
        }
        return impl;
    }

    private Impl loadImpl(Context context) {
        if (!isParentProcess(context)) {
            return null;
        }

        try {
            Context splitContext = getSplitContext(context);
            Class<?> implClass = splitContext.getClassLoader().loadClass(IMPL_CLASS_NAME);
            Object instance = implClass.getDeclaredConstructor().newInstance();
            return (Impl) instance;
        } catch (ClassNotFoundException | ClassCastException | IllegalAccessException |
                 InstantiationException | NoSuchMethodException | InvocationTargetException |
                 PackageManager.NameNotFoundException e) {
            throw new IllegalStateException("Failed to load FenixApplication from split " + APP_SPLIT_NAME, e);
        }
    }

    private Context getSplitContext(Context context) throws PackageManager.NameNotFoundException {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            return context.createContextForSplit(APP_SPLIT_NAME);
        }
        return context;
    }

    private boolean isParentProcess(Context context) {
        return context.getPackageName().equals(getCurrentProcessName(context));
    }

    private String getCurrentProcessName(Context context) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            return Application.getProcessName();
        }

        ActivityManager activityManager =
            (ActivityManager) context.getSystemService(Context.ACTIVITY_SERVICE);
        if (activityManager != null) {
            List<ActivityManager.RunningAppProcessInfo> runningAppProcesses =
                activityManager.getRunningAppProcesses();
            if (runningAppProcesses != null) {
                int pid = Process.myPid();
                for (ActivityManager.RunningAppProcessInfo processInfo : runningAppProcesses) {
                    if (processInfo.pid == pid) {
                        return processInfo.processName;
                    }
                }
            }
        }

        return context.getApplicationInfo().processName;
    }
}
