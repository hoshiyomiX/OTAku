package com.hoshiyomi.otaku

import android.content.Context
import android.widget.ArrayAdapter
import android.widget.Filter

/**
 * ArrayAdapter whose Filter is a strict pass-through, for fixed-choice
 * ExposedDropdownMenus (MaterialAutoCompleteTextView + inputType=none).
 *
 * T34 bug: the stock ArrayAdapter Filter performs PREFIX matching with the
 * field's current text as the constraint. When the framework filter path
 * runs while the field already holds the selection, the only surviving item
 * is the one that starts with the entire current text — i.e. the currently
 * selected option itself. On device this collapsed the compression
 * dropdown to a single row ("gzip", the persisted default), hiding
 * zstd/xz/bzip2/lz4. The same latent bug sat in the level dropdown and all
 * three payload-dialog dropdowns (e.g. level "Default (6)" would narrow the
 * list to just "Default (6)").
 *
 * For a fixed-choice dropdown the filter is meaningless — every reachable
 * value already comes from the adapter. Returning the full item set from
 * performFiltering() makes the list immune to filtering no matter which
 * framework path triggers it (identical behavior across Android versions
 * and OEM modifications).
 *
 * Pair with R.layout.item_dropdown_menu (48dp touch targets, M3 text
 * appearance) — replaces android.R.layout.simple_list_item_1 rows.
 */
class NoFilterArrayAdapter<T>(
    context: Context,
    resource: Int,
    items: List<T>
) : ArrayAdapter<T>(context, resource, items) {

    // Defensive copy: publishResults must never resurrect a mutated list.
    private val allItems: List<T> = items.toList()

    private val passThroughFilter = object : Filter() {
        override fun performFiltering(constraint: CharSequence?): FilterResults =
            FilterResults().apply {
                values = allItems
                count = allItems.size
            }

        override fun publishResults(constraint: CharSequence?, results: FilterResults?) {
            // Deliberately leaves the adapter contents untouched: the full
            // set is already the visible set. The Filter machinery still
            // drives onFilterComplete(), so popup show/hide logic upstream
            // (updateDropDownForFilter) keeps working.
        }
    }

    override fun getFilter(): Filter = passThroughFilter
}
