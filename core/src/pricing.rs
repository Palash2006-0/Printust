//! Pure integer-paise print-shop slab pricing engine.

use crate::models::{ColorMode, DuplexMode, PricingRules};

/// Calculate the exact print job cost in integer `i64` paise (`Rs. 1 = 100 paise`).
///
/// # Arguments
/// * `pages_per_copy` - Number of pages/sides in **one** copy of the document (never pass
///   an already-unrolled `effective_pages` total alongside `copies > 1`).
/// * `copies` - Number of copies printed.
/// * `is_color` - `true` for color rate slabs, `false` for B&W rate slabs.
/// * `is_duplex` - `true` for 2-sided sheet pricing, `false` for 1-sided pricing.
/// * `rules` - Shop pricing rules stored strictly as integer `i64` paise.
///
/// # Pricing Rules
/// 1. `total_sides = pages_per_copy.saturating_mul(copies)`. Returns `0` if `total_sides == 0`.
/// 2. **B&W Bulk Slab:** If `!is_color` and `rules.bw_bulk_threshold_sides > 0` and
///    `total_sides >= rules.bw_bulk_threshold_sides`, charges `total_sides * rules.bw_bulk_side_paise`.
///    Color jobs never use the B&W bulk slab.
/// 3. **Duplex (`is_duplex == true`):** On a physical printer, each copy starts on a new
///    sheet—Copy 2's page 1 never shares a sheet with Copy 1's trailing odd page. Therefore,
///    duplex sheets and any trailing odd single sheet are computed **per copy** (`pages_per_copy / 2`
///    and `pages_per_copy % 2`), then multiplied by `copies`.
/// 4. **Simplex (`is_duplex == false`):** Charges each side (`total_sides`) at the 1-sided sheet rate.
pub fn calculate_job_cost_paise(
    pages_per_copy: u32,
    copies: u32,
    is_color: bool,
    is_duplex: bool,
    rules: &PricingRules,
) -> i64 {
    let total_sides = pages_per_copy.saturating_mul(copies);
    if total_sides == 0 {
        return 0;
    }

    if !is_color
        && rules.bw_bulk_threshold_sides > 0
        && total_sides >= rules.bw_bulk_threshold_sides
    {
        return i64::from(total_sides).saturating_mul(rules.bw_bulk_side_paise);
    }

    let (single_rate, duplex_rate) = if is_color {
        (rules.color_single_paise, rules.color_duplex_sheet_paise)
    } else {
        (rules.bw_single_paise, rules.bw_duplex_sheet_paise)
    };

    if is_duplex {
        let full_duplex_per_copy = i64::from(pages_per_copy / 2);
        let remaining_single_per_copy = i64::from(pages_per_copy % 2);
        let cost_per_copy = full_duplex_per_copy
            .saturating_mul(duplex_rate)
            .saturating_add(remaining_single_per_copy.saturating_mul(single_rate));
        cost_per_copy.saturating_mul(i64::from(copies))
    } else {
        i64::from(total_sides).saturating_mul(single_rate)
    }
}

/// Calculate print job cost directly from explicit `ColorMode` and `DuplexMode` metadata.
///
/// Fallback policy for race conditions:
/// - `DuplexMode::Unknown` (when a fast unpaused job exits `GetJobW` before `DEVMODEW` is read)
///   safely bills at the simplex rate while `DEVMODE_RACE_MISSED` surfaces in the Mini-HUD.
/// - `ColorMode::Unknown` safely bills at the monochrome rate rather than guessing color.
pub fn calculate_job_cost_for_modes(
    pages_per_copy: u32,
    copies: u32,
    color_mode: ColorMode,
    duplex_mode: DuplexMode,
    rules: &PricingRules,
) -> i64 {
    calculate_job_cost_paise(
        pages_per_copy,
        copies,
        color_mode.is_color_billable(),
        duplex_mode.is_duplex_billable(),
        rules,
    )
}
