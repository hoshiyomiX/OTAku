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
 *   - [getSystemAccentColor]    — returns the user's wallpaper-derived accent
 *   - [shouldUseDynamicTheme]   — true if API 31+ AND user hasn't disabled it
 *
 * THEME-DEFAULT-FIX: applyDynamicTheme() now ALWAYS uses Theme.OTAku.Suisei
 * as the base theme (Suisei Blue #00B0F0 as the brand default), then applies
 * DynamicColors on top when available. This means:
 *   - API 31+ with dynamic color enabled: Suisei Blue base + Material You overlay
 *     (system accent, including cyan, is applied without restriction)
 *   - API 26-30 or dynamic color disabled: Suisei Blue palette only
 * The generic teal/cyan Theme.OTAku is NO LONGER used as the default.
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

    /**
     * Resolve the system's primary accent color (Material You palette slot 1).
     *
     * On API 31+, returns `android.R.color.system_accent1_500` — the
     * middle tone of the user's wallpaper-derived accent palette. This is
     * the closest equivalent to "the user's chosen accent color" that
     * Android exposes publicly.
     *
     * On API 26-30, returns the Suisei Blue seed color (#00B0F0) directly.
     *
     * On API 31+ if the lookup fails (rare — some heavily-customized OEM
     * ROMs strip the system_* color resources), falls back to Suisei Blue.
     *
     * @param context Any context — used to resolve the system color resource
     * @return The accent color as a 0xAARRGGBB int (alpha always 0xFF)
     */
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


    /**
     * Whether the app should apply the dynamic Material You theme.
     *
     * True when ALL of:
     *   1. Device is API 31+ (Material You available)
     *   2. User hasn't explicitly disabled dynamic color in app settings
     *
     * When false, MainActivity uses Theme.OTAku.Suisei (Suisei Blue palette)
     * instead of Theme.OTAku (default teal, which DynamicColors would
     * override on API 31+).
     *
     * @param prefs The app's SharedPreferences ("otaku" preferences)
     */
    fun shouldUseDynamicTheme(prefs: android.content.SharedPreferences): Boolean {
        // Default: dynamic color ON if available. Users on older devices
        // get Suisei Blue; users on API 31+ get Material You unless they
        // explicitly opt out via Settings (future feature — currently no UI).
        val userEnabled = prefs.getBoolean("pref_use_dynamic_color", true)
        return isDynamicColorAvailable && userEnabled
    }
}