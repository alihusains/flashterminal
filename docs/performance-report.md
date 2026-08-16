# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-16 10:43:36 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 8.97 MB | 9.03 MB | 40.00 MB | +0.06 MB | pass |
| ten_panes_ram_mb | 11.30 MB | 11.30 MB | 80.00 MB | +0.00 MB | pass |
| input_latency_apply_p95_ms | 4.20 ms | 3.76 ms | 8.00 ms | -0.44 ms | pass |
| parse_1m_lines_ms | 754.17 ms | 625.20 ms | 0.00 ms | -128.97 ms | pass |
| parse_10m_lines_ms | 10061.27 ms | 11830.72 ms | 0.00 ms | +1769.45 ms | pass |
| snapshot_frame_us | 6.14 µs | 12.16 µs | 0.00 µs | +6.02 µs | pass |
| render_prep_10k_rows_ms | 4.46 ms | 3.65 ms | 0.00 ms | -0.80 ms | pass |
| glyph_raster_us_per_glyph | 0.13 µs | 0.11 µs | 0.00 µs | -0.02 µs | pass |
| scrollback_10k_rows_ms | 15.16 ms | 16.59 ms | 0.00 ms | +1.43 ms | pass |
| unicode_write_ns_per_char | 5.94 ns | 5.32 ns | 0.00 ns | -0.61 ns | pass |
| grid_10m_cells_mb | 196.03 MB | 190.00 MB | 0.00 MB | -6.03 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-16 10:43:36 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 9.03125,
  "ten_panes_ram_mb": 11.296875,
  "input_latency_apply_p95_ms": 3.7594160000000003,
  "parse_1m_lines_ms": 625.202083,
  "parse_10m_lines_ms": 11830.71725,
  "grid_10m_cells_mb": 190.0,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 5.323749999999999,
  "snapshot_frame_us": 12.15959,
  "render_prep_10k_rows_ms": 3.6545829999999997,
  "glyph_raster_us_per_glyph": 0.10784549549549549,
  "scrollback_10k_rows_ms": 16.594167
}
```
