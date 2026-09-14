package com.hoshiyomi.otaku

import android.content.Context
import android.content.res.Configuration
import android.os.Build
import android.util.Log
import androidx.annotation.ColorInt
import androidx.core.content.ContextCompat

/**
 * SuiseiColors — system accent color detector with Suisei Blue fallback.
 *
 * Provides the system's Material You accent color (Android 12+ / API 31+)
 * for use in dynamic theming. On older Android versions (API 26-30, the
 * app's minSdk), or if the system color lookup fails for any reason, falls
 * back to "Suisei Blue" — Hoshimachi Suisei's signature vivid cyan-blue
 * (#00B0F0).
 *
 * Two main entry points:
 *   - [getSystemAccentColor]       — returns the user's wallpaper-derived accent
 *   - [isDynamicColorAvailable]    — true on API 31+ (Material You capable)
 *
 * THEME-DEFAULT-FIX: MainActivity.onCreate() ALWAYS sets Theme.OTAku.Suisei
 * as the base theme (Suisei Blue #00B0F0 as the brand default) — after
 * super.onCreate() and applyTheme(), per the IMPL-007/IMPL-008 ordering —
 * then applyDynamicColorsOverlay() applies DynamicColors on top when
 * isDynamicColorAvailable is true. This means:
 *   - API 31+: Suisei Blue base + Material You overlay
 *     (system accent, including cyan, is applied without restriction)
 *   - API 26-30: Suisei Blue palette only
 * The generic teal/cyan Theme.OTAku is NO LONGER used as the default.
 *
 * AUDIT-DC: dynamic color is pure auto-detect — the user-facing toggle
 * in Settings and its backing preference key were removed.
 * Material You is applied automatically on every device that supports it
 * (Android 12+); older devices always get the Suisei Blue brand palette.
 *
 * Why not just use DynamicColors.applyToActivityIfAvailable()?
 *   - That API only colors Material3 components that opt in via
 *     ?attr/colorPrimary etc. It doesn't override our base palette.
 *   - We want a clear either/or: Material You overlay (when available) on
 *     top of Suisei Blue base, or Suisei Blue alone (when not).
 *   - The theme-overlay approach gives us full control of every color slot
 *     (primary, secondary, tertiary, surface, error, etc.) and works
 *     consistently across all UI components.
 */
object SuiseiColors {

    private const val TAG = "SuiseiColors"


    /**
     * Whether the device supports Material You dynamic color.
     *
     * Material You (system accent color from wallpaper) was introduced in
     * Android 12.0 (API 31, S). Earlier versions don't expose
     * android.R.color.system_accent1_* — attempting to resolve those
     * resources on API 30 or below will throw ResourcesNotFoundException.
     */
    val isDynamicColorAvailable: Boolean
        get() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S

    /** Suisei Blue literal — used as fallback when system accent is unavailable. */
    private const val SUISEI_BLUE = 0xFF00B0F0.toInt()

    /**
     * Resolve the system's primary accent color (Material You palette slot 1).
     *
     * On API 31+, returns `android.R.color.system_accent1_500` — the
     * mid-tone of the user's wallpaper-derived accent palette.
     * On API 26-30 or if lookup fails, returns Suisei Blue (#00B0F0).
     */
    @ColorInt
    fun getSystemAccentColor(context: Context): Int {
        if (!isDynamicColorAvailable) {
            return SUISEI_BLUE
        }
        return try {
            ContextCompat.getColor(context, android.R.color.system_accent1_500)
        } catch (e: Exception) {
            Log.w(TAG, "system_accent1_500 unavailable, falling back to Suisei Blue: ${e.message}")
            SUISEI_BLUE
        }
    }

}
