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
 * Used by MainActivity.applyDynamicTheme() to decide which theme overlay
 * to apply: Theme.OTAku (Material You, default teal) on API 31+ when dynamic
 * color is enabled, or Theme.OTAku.Suisei (Suisei Blue palette) on older
 * devices or when dynamic color is disabled.
 *
 * Why not just use DynamicColors.applyToActivityIfAvailable()?
 *   - That API only colors Material3 components that opt in via
 *     ?attr/colorPrimary etc. It doesn't override our existing teal palette.
 *   - We want a clear either/or: Material You (when available) OR Suisei
 *     Blue (when not). Not a hybrid.
 *   - The theme-overlay approach gives us full control of every color slot
 *     (primary, secondary, tertiary, surface, error, etc.) and works
 *     consistently across all UI components.
 *
 * Why a separate Suisei palette instead of just shifting the existing teal?
 *   - The existing teal (#006B5A) is a deliberate brand choice. When the
 *     user can't get Material You, we honor the Suisei reference rather
 *     than leave them with a color they didn't choose.
 *   - The Suisei palette is a full Material 3 tonal palette generated from
 *     the #00B0F0 seed color, ensuring all contrast ratios meet WCAG AA.
 */
object SuiseiColors {

    private const val TAG = "SuiseiColors"

    /**
     * Hoshimachi Suisei's signature blue — the seed color for the fallback palette.
     *
     * Source: official character art / merchandise. This is the most commonly
     * cited hex for her hair/accessory blue.
     */
    const val SUISEI_BLUE_SEED = 0xFF00B0F0.toInt()

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
    @ColorInt
    fun getSystemAccentColor(context: Context): Int {
        if (!isDynamicColorAvailable) {
            return SUISEI_BLUE_SEED
        }
        return try {
            // system_accent1_500 is the canonical "user's primary accent" slot.
            // It's a mid-tone that works for both light and dark themes.
            // See: https://developer.android.com/about/versions/12/features#material-you
            ContextCompat.getColor(context, android.R.color.system_accent1_500)
        } catch (e: Exception) {
            // Defensive: some OEM ROMs (rare) strip the system_* color resources.
            // Log and fall back rather than crash.
            Log.w(TAG, "system_accent1_500 unavailable, falling back to Suisei Blue: ${e.message}")
            SUISEI_BLUE_SEED
        }
    }

    /**
     * Resolve the system's primary container color (lighter tint of accent).
     *
     * Used for backgrounds of prominent components (chips, buttons, dialogs).
     * On API 31+ returns `system_accent1_100` (lightest tone). On older
     * versions, returns the Suisei palette's primaryContainer color.
     *
     * @param context Any context
     * @return The container color as a 0xAARRGGBB int
     */
    @ColorInt
    fun getSystemAccentContainerColor(context: Context): Int {
        if (!isDynamicColorAvailable) {
            // Return the light variant of Suisei Blue for container use.
            // We can't reference @color/suisei_light_primaryContainer from
            // here without going through ContextCompat.getColor(context, R.color.suisei_light_primaryContainer),
            // but for simplicity we use a hardcoded value matching the
            // colors.xml definition. If colors.xml changes, update both.
            return 0xFFD1E4FF.toInt()  // suisei_light_primaryContainer
        }
        return try {
            ContextCompat.getColor(context, android.R.color.system_accent1_100)
        } catch (e: Exception) {
            Log.w(TAG, "system_accent1_100 unavailable: ${e.message}")
            0xFFD1E4FF.toInt()
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

    /**
     * Human-readable description of the current color source.
     *
     * For diagnostics / about screen. Returns one of:
     *   - "Material You (system accent)" — API 31+ with dynamic color enabled
     *   - "Suisei Blue (default)"        — fallback on older devices or user-disabled
     */
    fun describeColorSource(prefs: android.content.SharedPreferences): String {
        return if (shouldUseDynamicTheme(prefs)) {
            "Material You (system accent)"
        } else {
            "Suisei Blue (default)"
        }
    }

    /**
     * Determine whether the current configuration is in dark mode.
     *
     * Utility for callers that need to pick light vs dark variants of
     * hardcoded fallback colors. The system theme overlay handles this
     * automatically for theme attributes, but utility code that resolves
     * colors directly needs to check.
     */
    fun isNightMode(context: Context): Boolean {
        val nightMode = context.resources.configuration.uiMode and
            Configuration.UI_MODE_NIGHT_MASK
        return nightMode == Configuration.UI_MODE_NIGHT_YES
    }
}
