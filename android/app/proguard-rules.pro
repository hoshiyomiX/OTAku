# ProGuard rules for OTAku v2.1
# ================================

# Keep Kotlin coroutines
-keepnames class kotlinx.coroutines.internal.MainDispatcherFactory {}
-keepnames class kotlinx.coroutines.CoroutineExceptionHandler {}

# Keep bridge classes (called from UI)
-keep class com.hoshiyomi.otaku.OTABridge { *; }
-keep class com.hoshiyomi.otaku.OTAResult { *; }
-keep class com.hoshiyomi.otaku.PythonBridge { *; }
-keep class com.hoshiyomi.otaku.ExecResult { *; }
-keep class com.hoshiyomi.otaku.ProgressUpdate { *; }

# Keep OTAkuApp (declared in AndroidManifest)
-keep class com.hoshiyomi.otaku.OTAkuApp { *; }

# Keep service classes (declared in AndroidManifest)
-keep class com.hoshiyomi.otaku.service.** { *; }

# Keep data classes (declared in AndroidManifest backup)
-keep class com.hoshiyomi.otaku.data.** { *; }

# AndroidX
-keep class androidx.** { *; }
-dontwarn androidx.**

# Material Design
-keep class com.google.android.material.** { *; }
-dontwarn com.google.android.material.**
