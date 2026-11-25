# Driver Lie Matrix — Phase 1 (Days 1 & 2)

Empirical verification of Windows Print Spooler Event IDs `805` (`RenderJobDiag`) and `307` (`DocumentPrinted`) against the silent `NUL`-port test printer (`ShopTest_PCL6` using `HP Universal Printing PCL 6`).

## Test Protocol & Observed Results

| Case # | Test Scenario | Source App | Expected Physical Sheets | Event 805 (`Copies` / `Color` / `Raw`) | Event 307 (`PagesPrinted` / `Bytes`) | Driver Lied? (Y/N) | Root Cause / Heuristic Note |
| :--- | :--- | :--- | :---: | :--- | :--- | :---: | :--- |
| **1** | 1-page PDF × 1 copy (B&W) | Chrome | 1 | `JobId=3`: `Copies=1` / `Color=1` / `GdiJobSize=450409`, `XRes=300`, `YRes=300`, `Quality=300`, `ICMMethod=1`, `TTOption=3` | `1` / `450409` (`Rec 283`) | **N** | Baseline sanity check passed (`PagesPrinted=1`, `Color=1` B&W). |
| **2** | 3-page PDF × 1 copy (B&W) | Chrome | 3 | `JobId=4`: `Copies=1` / `Color=1` / `GdiJobSize=1332413` *(plus repeat `JobId=6`: `Copies=1`, `Color=1`, `GdiJobSize=1332413`)* | `3` / `1332413` (`Rec 288`; repeat `Rec 298`) | **N** | Multi-page count accurate (`PagesPrinted=3`). |
| **3** | 3-page PDF × 3 copies (B&W) | Chrome | 9 | `JobId=10`: `Copies=3` / `Color=1` / `GdiJobSize=9240576`, `XRes=300`, `YRes=300`, `Quality=300`, `ICMMethod=1`, `TTOption=3` | `9` / `9240576` (`Rec 318`) | **N** | **Critical finding:** Chrome + `HP Universal Printing PCL 6` unrolls all 9 pages in EMF/GDI (`9.24 MB`), so Event 307 `PagesPrinted=9` **already includes `Copies=3`** (`3 × 3 = 9`). Do **not** multiply `PagesPrinted * Copies`. |
| **4** | 4-page PDF × 1 copy (Duplex / 2-sided) | Chrome | 2 sheets (4 sides) | `JobId=7`: `Copies=1` / `Color=1` / `GdiJobSize=1787412` *(no `Duplex` tag present)* | `4` / `1787412` (`Rec 303`) | **Y** | **Driver lied on physical sheets:** Event 307 reported `4` sides instead of `2` physical sheets, and Event 805 omitted Duplex entirely. Day 3 `.SHD` (`DEVMODE.dmDuplex`) inspection is required. |
| **5** | 2-page PDF × 1 copy (Color vs Grayscale) | Chrome | 2 | `JobId=8` (Color): `Copies=1` / `Color=2` / `GdiJobSize=952995` *(vs B&W `Color=1` in Cases 1–4)* | `2` / `952995` (`Rec 308`) | **N** | Confirmed Windows `DEVMODE.dmColor` semantics: `Color=1` = Monochrome/B&W, `Color=2` = Color. |
| **6** | 2-page Doc × 2 copies (App comparison) | MS Word *(Non-Chromium)* | 4 | **PENDING** *(extra Chromium `2p × 2c` runs captured: `JobId=5` `Copies=2`/`Color=1`/`7012352`, `JobId=9` `Copies=2`/`Color=2`/`7012352`)* | **PENDING** *(extra Chromium runs: `4` / `7012352` at `Rec 293`, `Rec 313`)* | **PENDING** | Non-Chromium app (MS Word `.docx` × 2 copies) not yet executed; `JobId=5` & `JobId=9` both ran through Chromium (`DocumentName="Print Document"`). |

---

## Summary & Findings

- **Tested Printer & Driver Name (`ShopTest_PCL6`):** `HP Universal Printing PCL 6` (MajorVersion `3`, Manufacturer `HP`, Port `NUL`).
- **Event 307 `PagesPrinted` vs Event 805 `Copies`:** Across all captured Chromium jobs (`JobId 3`–`10`), Event `307` `PagesPrinted` **already multiplies by `Copies`** (`JobId 10`: `Copies=3`, `PagesPrinted=9`; `JobId 5` & `JobId 9`: `Copies=2`, `PagesPrinted=4`), and `GdiJobSize` scales with the unrolled copies (`9,240,576` bytes for 3 copies; `7,012,352` bytes for 2 copies). **Heuristic rule:** Never blindly multiply `PagesPrinted * Copies` when `PagesPrinted` already reflects unrolled impressions; verify non-Chromium spooling via Case 6 (MS Word) and Day 3 `.SHD` + `.SPL` inspection.
- **Duplex Detection & Day 3 `.SHD` / `DEVMODE` Requirement:** Confirmed **required**. Across all 8 captured jobs (`JobId 3`–`10`), Event `805` emitted only `{JobId, GdiJobSize, ICMMethod, Color, XRes, YRes, Quality, Copies, TTOption}` with **zero Duplex fields**. `JobId 7` (4-page Duplex) reported `PagesPrinted=4` (sides) instead of `2` physical sheets, so physical sheet calculation (`ceil(sides / 2)`) must read `DEVMODE.dmDuplex` from the Windows spooler `.SHD` shadow file.
- **Color Field Semantics:** Confirmed empirically on `HP Universal Printing PCL 6`: `Color=1` = Black & White / Grayscale (`JobId 3, 4, 5, 6, 7, 10`), `Color=2` = Color (`JobId 8, 9`). Note also that `GdiJobSize` in `JobId 5` (`Color=1`) and `JobId 9` (`Color=2`) was identical (`7,012,352` bytes).
- **App-Specific Spooling Notes:** Chromium-based printing sets Event `307` `Param2` (`DocumentName`) to generic `"Print Document"` across all captured jobs (`JobId 3`–`10`), masking source PDF filenames. Case 6 (MS Word) remains pending to verify non-Chromium behavior.
