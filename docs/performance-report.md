# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-16 14:39:36 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 9.02 MB | 9.00 MB | 40.00 MB | -0.02 MB | pass |
| ten_panes_ram_mb | 11.31 MB | 11.38 MB | 80.00 MB | +0.06 MB | pass |
| input_latency_apply_p95_ms | 0.00 ms | 0.00 ms | 0.00 ms | +0.00 ms | pass |
| parse_1m_lines_ms | 639.76 ms | 608.78 ms | 0.00 ms | -30.98 ms | pass |
| parse_10m_lines_ms | 11312.04 ms | 11165.97 ms | 0.00 ms | -146.07 ms | pass |
| snapshot_frame_us | 12.24 µs | 11.62 µs | 0.00 µs | -0.62 µs | pass |
| render_prep_10k_rows_ms | 3.66 ms | 3.61 ms | 0.00 ms | -0.04 ms | pass |
| glyph_raster_us_per_glyph | 0.10 µs | 0.10 µs | 0.00 µs | -0.01 µs | pass |
| scrollback_10k_rows_ms | 15.76 ms | 17.53 ms | 0.00 ms | +1.77 ms | pass |
| unicode_write_ns_per_char | 4.47 ns | 4.59 ns | 0.00 ns | +0.12 ns | pass |
| grid_10m_cells_mb | 190.00 MB | 196.03 MB | 0.00 MB | +6.03 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-16 14:39:36 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 9.0,
  "ten_panes_ram_mb": 11.375,
  "input_latency_apply_p95_ms": 0.000042000000000000004,
  "parse_1m_lines_ms": 608.7804580000001,
  "parse_10m_lines_ms": 11165.966334,
  "grid_10m_cells_mb": 196.03125,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 4.59109375,
  "snapshot_frame_us": 11.62,
  "render_prep_10k_rows_ms": 3.611958,
  "glyph_raster_us_per_glyph": 0.09694054054054053,
  "scrollback_10k_rows_ms": 17.52975
}
```
