# ProGuard / R8 rules for OTAku
# ==============================
# These rules ensure R8 does not strip or obfuscate classes that are:
#   - Referenced from AndroidManifest.xml (backupAgent, Application, Service)
#   - Accessed via JNI (PyBridge native methods)
#   - Used as data transfer objects between Kotlin and Python
#   - Required for Kotlin coroutines and AndroidX internals

# ─── Preserve attributes needed for reflection, JNI, and serialization ───
-keepattributes *Annotation*
-keepattributes Signature
-keepattributes Exceptions
-keepattributes InnerClasses
-keepattributes EnclosingMethod

# ─── Kotlin coroutines ───
-keepnames class kotlinx.coroutines.internal.MainDispatcherFactory {}
-keepnames class kotlinx.coroutines.CoroutineExceptionHandler {}
-keepclassmembers class kotlinx.coroutines.** {
    volatile <fields>;
}

# ─── JNI Bridge: NativeBridge (Rust libotaku_native.so) ───
# NativeBridge loads libotaku_native.so via System.loadLibrary and declares native methods.
# R8 must NOT strip or obfuscate the class or its native method signatures.
-keep class com.hoshiyomi.otaku.NativeBridge { *; }
-keep class com.hoshiyomi.otaku.NativeBridge$Companion { *; }
-keep class com.hoshiyomi.otaku.NativeBridge$DepCheckResult { *; }
-keepclassmembers class com.hoshiyomi.otaku.NativeBridge {
    native <methods>;
}

# ─── JNI Bridge: PyBridge (legacy, Phase 4 removal) ───
# PyBridge loads libpybridge.so via System.loadLibrary and declares native methods.
# R8 must NOT strip or obfuscate the class or its native method signatures,
# otherwise the JNI linker cannot resolve native functions at runtime.
-keep class com.hoshiyomi.otaku.PyBridge { *; }
-keep class com.hoshiyomi.otaku.PyBridge$Companion { *; }
-keep class com.hoshiyomi.otaku.PyBridge$PyResult { *; }
-keepclassmembers class com.hoshiyomi.otaku.PyBridge {
    native <methods>;
}

# ─── OTA Bridge classes (Kotlin ↔ Python interface) ───
-keep class com.hoshiyomi.otaku.OTABridge { *; }
-keep class com.hoshiyomi.otaku.OTAResult { *; }
-keep class com.hoshiyomi.otaku.PythonBridge { *; }
-keep class com.hoshiyomi.otaku.PythonBridge$InitResult { *; }
-keep class com.hoshiyomi.otaku.PythonBridge$DepCheckResult { *; }
-keep class com.hoshiyomi.otaku.ExecResult { *; }
-keep class com.hoshiyomi.otaku.ProgressUpdate { *; }

# ─── Application class (declared in AndroidManifest) ───
-keep class com.hoshiyomi.otaku.OTAkuApp { *; }

# ─── MainActivity (declared in AndroidManifest) ───
# Companion object holds static state accessed from coroutines;
# keeping the whole class ensures no member is stripped.
-keep class com.hoshiyomi.otaku.MainActivity { *; }

# ─── Service class (declared in AndroidManifest with foregroundServiceType) ───
-keep class com.hoshiyomi.otaku.service.OTAService { *; }

# ─── Backup agent (declared in AndroidManifest android:backupAgent) ───
-keep class com.hoshiyomi.otaku.data.BackupAgent { *; }

# ─── BuildConfig (referenced from PythonBridge for version info) ───
-keep class com.hoshiyomi.otaku.BuildConfig { *; }

# ─── AndroidX ───
# Don't keep ALL of androidx (causes R8 optimizer conflicts).
# Instead, suppress warnings and let R8 shrink normally.
-dontwarn androidx.**
-keep class androidx.core.app.NotificationCompat$Builder { *; }
-keep class androidx.core.content.FileProvider { *; }

# ─── Material Design ───
-dontwarn com.google.android.material.**
-keep class com.google.android.material.appbar.MaterialToolbar { *; }
-keep class com.google.android.material.dialog.MaterialAlertDialogBuilder { *; }
-keep class com.google.android.material.textfield.TextInputEditText { *; }
-keep class com.google.android.material.progressindicator.LinearProgressIndicator { *; }

# ─── Kotlin metadata ───
# Required for Kotlin reflection and serialization
-keepattributes RuntimeVisibleAnnotations
-keep class kotlin.Metadata { *; }
-keepclassmembers class **$WhenMappings {
    <fields>;
}

# ─── Serializable / Parcelable support ───
# OTAService passes Map via Intent extras with getSerializableExtra
-keepclassmembers class * implements java.io.Serializable {
    static final long serialVersionUID;
    private static final java.io.ObjectStreamField[] serialPersistentFields;
    !static !transient <fields>;
    private void writeObject(java.io.ObjectOutputStream);
    private void readObject(java.io.ObjectInputStream);
    java.lang.Object writeReplace();
    java.lang.Object readResolve();
}
-keepclassmembers class * implements android.os.Parcelable {
    public static final ** CREATOR;
}

# ─── Disable obfuscation for this project ───
# OTAku is open-source; obfuscation provides no security benefit
# and makes debugging crash reports harder.
-dontobfuscate

# ─── General R8 safety ───
-dontwarn java.lang.invoke.StringConcatFactory
-dontwarn org.jetbrains.annotations.**
