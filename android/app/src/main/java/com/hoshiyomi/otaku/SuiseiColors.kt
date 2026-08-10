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
     * Hoshimachi Suisei's signature blue — the seed color for the fallback palette.
     *
     * Source: official character art / merchandise. This is the most commonly
     * cited hex for her hair/accessory blue.
     */
    @Deprecated(message = "Unused - not called from any Kotlin code. Available for future dynamic-theming UI features.")
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
    @Deprecated(message = "Unused - not called from any Kotlin code. Available for future dynamic-theming UI features.")
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
    @Deprecated(message = "Unused - not called from any Kotlin code. Available for future diagnostics/about-screen UI.")
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
    @Deprecated(message = "Unused - not called from any Kotlin code. Available for future dark-mode-aware color resolution.")
    fun isNightMode(context: Context): Boolean {
        val nightMode = context.resources.configuration.uiMode and
            Configuration.UI_MODE_NIGHT_MASK
        return nightMode == Configuration.UI_MODE_NIGHT_YES
    }

    /**
     * Detect whether a color is "cyan-like" — a generic cyan/teal system accent.
     *
     * @Deprecated This method is NO LONGER used to reject system accent colors.
     * THEME-DEFAULT-FIX: The app now always uses Theme.OTAku.Suisei as the base
     * theme and applies DynamicColors on top — ALL system accent colors (including
     * cyan) are applied without restriction. The user chose their wallpaper, and
     * we respect it. This method is retained as a utility for potential future
     * diagnostics/UI features but should NOT be used for accent filtering.
     *
     * Detection uses HSL (Hue-Saturation-Lightness) color space:
     *   - Hue: 175°–200° (cyan range — excludes blue ~224° like Suisei Blue
     *     and green ~120° like the brand teal)
     *   - Saturation: > 40% (excludes desaturated grays)
     *   - Lightness: > 30% (excludes dark teals like the brand color #006B5A
     *     which has L ≈ 21%)
     *
     * Additional RGB proximity check: colors within 25 RGB units of Suisei Blue
     * (#00B0F0) are NOT classified as cyan-like (they're close to our brand color).
     *
     * @param color ARGB color int (0xAARRGGBB)
     * @return true if the color is generic cyan (for diagnostic purposes only)
     */
    @Deprecated(message = "Not used for accent filtering — all system accents are applied. Retained as diagnostic utility.")
    fun isCyanLike(@ColorInt color: Int): Boolean {
        // Extract RGB channels (0–255)
        val r = (color shr 16 and 0xFF) / 255f
        val g = (color shr 8 and 0xFF) / 255f
        val b = (color and 0xFF) / 255f

        // RGB → HSL conversion
        val max = maxOf(r, g, b)
        val min = minOf(r, g, b)
        val l = (max + min) / 2f  // lightness [0,1]

        if (max == min) {
            // Achromatic (gray) — no hue, not cyan
            return false
        }

        val d = max - min
        val s = if (l > 0.5f) d / (2f - max - min) else d / (max + min)  // saturation [0,1]

        val h = when (max) {
            r -> ((g - b) / d + if (g < b) 6f else 0f) / 6f
            g -> ((b - r) / d + 2f) / 6f
            else -> ((r - g) / d + 4f) / 6f
        }  // hue [0,1]

        val hueDegrees = h * 360f
        val saturationPercent = s * 100f
        val lightnessPercent = l * 100f

        // Cyan: hue 175–200°, saturation > 40%, lightness > 30%
        // This range catches Material Cyan (#00BCD4, hue ~187°) and similar
        // OEM cyan accents, while excluding:
        //   - Suisei Blue (#00B0F0, hue ~198° in sRGB... let me verify)
        //     Actually, #00B0F0 in HSL: hue ~198° — this IS in range!
        //     But Suisei Blue has a distinctively different character from
        //     generic cyan (more saturated, more blue-shifted). We use an
        //     additional RGB proximity check to exclude Suisei Blue itself.
        //
        // RGB proximity to Suisei Blue (#00B0F0):
        //   If the color is within 25 RGB units of Suisei Blue, it's NOT
        //   cyan-like — it's close to our brand color and should be kept.
        //   Threshold: 25 (Material Cyan #00BCD4 is ~30.5 units away and
        //   should NOT be excluded; only near-exact Suisei variants like
        //   #03A9F4 at ~8.6 units should be kept).
        val suiseiR = 0x00; val suiseiG = 0xB0; val suiseiB = 0xF0
        val dr = (color shr 16 and 0xFF) - suiseiR
        val dg = (color shr 8 and 0xFF) - suiseiG
        val db = (color and 0xFF) - suiseiB
        val rgbDistance = kotlin.math.sqrt((dr * dr + dg * dg + db * db).toFloat())

        if (rgbDistance < 25f) {
            // Close to Suisei Blue — keep it, not generic cyan
            return false
        }

        return hueDegrees in 175f..200f && saturationPercent > 40f && lightnessPercent > 30f
    }
}
