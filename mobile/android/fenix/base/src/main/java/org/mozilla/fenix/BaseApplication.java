/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

package org.mozilla.fenix;

import android.app.ActivityManager;
import android.app.Application;
import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.os.Build;
import android.os.Process;

import androidx.work.Configuration.Builder;
import androidx.work.Configuration.Provider;

import dalvik.system.BaseDexClassLoader;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.util.List;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

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

    private void configureJna(Context context, ClassLoader classLoader)
            throws NoSuchFieldException, IllegalAccessException, IOException {
        File extractedDispatchLibrary = extractLibrary(context, "jnidispatch");
        if (extractedDispatchLibrary == null) {
            throw new IllegalStateException("Failed to resolve libjnidispatch.so from base");
        }

        File extractedMegazordLibrary = extractLibrary(context, "megazord");
        if (extractedMegazordLibrary == null) {
            throw new IllegalStateException("Failed to resolve libmegazord.so from base");
        }

        String extractedLibraryDir = extractedDispatchLibrary.getParent();
        System.setProperty("jna.boot.library.path", extractedLibraryDir);
        System.setProperty("jna.library.path", extractedLibraryDir);
    }

    private Impl loadImpl(Context context) {
        if (!isParentProcess(context)) {
            return null;
        }

        try {
            Context splitContext = getSplitContext(context);
            configureJna(context, splitContext.getClassLoader());
            Class<?> implClass = splitContext.getClassLoader().loadClass(IMPL_CLASS_NAME);
            Object instance = implClass.getDeclaredConstructor().newInstance();
            return (Impl) instance;
        } catch (ClassCastException | IllegalAccessException |
                 InstantiationException | NoSuchMethodException | InvocationTargetException |
                 NoSuchFieldException | PackageManager.NameNotFoundException | IOException |
                 ClassNotFoundException e) {
            throw new IllegalStateException("Failed to load FenixApplication from split " + APP_SPLIT_NAME, e);
        }
    }

    private String getNativeLibraryPath(Context context, String libraryName)
            throws NoSuchFieldException, IllegalAccessException {
        String path = findLibraryPath(BaseApplication.class.getClassLoader(), libraryName);
        if (path != null) {
            return path;
        }

        path = findLibraryPath(context.getClassLoader(), libraryName);
        if (path != null) {
            return path;
        }

        return getApkLibraryPath(context.getApplicationInfo(), libraryName);
    }

    private String findLibraryPath(ClassLoader classLoader, String libraryName) {
        if (classLoader instanceof BaseDexClassLoader) {
            return ((BaseDexClassLoader) classLoader).findLibrary(libraryName);
        }
        return null;
    }

    private String getApkLibraryPath(ApplicationInfo applicationInfo, String libraryName)
            throws NoSuchFieldException, IllegalAccessException {
        String primaryCpuAbi = (String) applicationInfo.getClass().getField("primaryCpuAbi")
                .get(applicationInfo);
        if (primaryCpuAbi == null) {
            return null;
        }

        String abiSplitPath = getAbiSplitPath(applicationInfo, primaryCpuAbi);
        String apkPath = abiSplitPath != null ? abiSplitPath : applicationInfo.sourceDir;
        return apkPath + "!/lib/" + primaryCpuAbi + "/" + System.mapLibraryName(libraryName);
    }

    private String getAbiSplitPath(ApplicationInfo applicationInfo, String primaryCpuAbi) {
        String[] splitNames = applicationInfo.splitNames;
        String[] splitSourceDirs = applicationInfo.splitSourceDirs;
        if (splitNames == null || splitSourceDirs == null) {
            return null;
        }

        String abiToken = primaryCpuAbi.replace('-', '_');
        String baseConfigSplitName = "config." + abiToken;
        for (int i = 0; i < splitNames.length && i < splitSourceDirs.length; i++) {
            String splitName = splitNames[i];
            if (baseConfigSplitName.equals(splitName)) {
                return splitSourceDirs[i];
            }
        }

        String baseConfigApkName = "split_config." + abiToken + ".apk";
        for (String splitSourceDir : splitSourceDirs) {
            if (splitSourceDir != null && splitSourceDir.endsWith("/" + baseConfigApkName)) {
                return splitSourceDir;
            }
        }

        for (String splitSourceDir : splitSourceDirs) {
            if (splitSourceDir != null && splitSourceDir.contains("split_config." + abiToken + ".apk")) {
                return splitSourceDir;
            }
        }

        for (int i = 0; i < splitNames.length && i < splitSourceDirs.length; i++) {
            String splitName = splitNames[i];
            if (splitName != null && splitName.endsWith("." + abiToken)) {
                return splitSourceDirs[i];
            }
        }

        return null;
    }

    private File extractLibrary(Context context, String libraryName)
            throws NoSuchFieldException, IllegalAccessException, IOException {
        String path = getNativeLibraryPath(context, libraryName);
        if (path == null) {
            return null;
        }

        if (!path.contains(".apk!/")) {
            return new File(path);
        }

        int separatorIndex = path.indexOf("!/");
        String apkPath = path.substring(0, separatorIndex);
        String entryName = path.substring(separatorIndex + 2);
        File outputDir = new File(context.getDir("jna", Context.MODE_PRIVATE), "lib");
        if (!outputDir.exists() && !outputDir.mkdirs()) {
            throw new IOException("Failed to create JNA output directory");
        }

        File outputFile = new File(outputDir, new File(entryName).getName());
        try (ZipFile zipFile = new ZipFile(apkPath)) {
            ZipEntry entry = zipFile.getEntry(entryName);
            if (entry == null) {
                throw new IOException("Missing " + entryName + " in " + apkPath);
            }

            if (outputFile.exists() && outputFile.length() == entry.getSize()) {
                return outputFile;
            }

            try (InputStream inputStream = zipFile.getInputStream(entry);
                 FileOutputStream outputStream = new FileOutputStream(outputFile)) {
                byte[] buffer = new byte[8192];
                int read;
                while ((read = inputStream.read(buffer)) != -1) {
                    outputStream.write(buffer, 0, read);
                }
            }
        }

        return outputFile;
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
