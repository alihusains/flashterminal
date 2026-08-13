# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-12 11:29:06 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 8.95 MB | 8.97 MB | 40.00 MB | +0.02 MB | pass |
| ten_panes_ram_mb | 11.12 MB | 11.30 MB | 80.00 MB | +0.17 MB | pass |
| input_latency_apply_p95_ms | 4.20 ms | 4.20 ms | 8.00 ms | -0.00 ms | pass |
| parse_1m_lines_ms | 760.23 ms | 754.17 ms | 0.00 ms | -6.06 ms | pass |
| parse_10m_lines_ms | 10054.96 ms | 10061.27 ms | 0.00 ms | +6.31 ms | pass |
| snapshot_frame_us | 5.46 µs | 6.14 µs | 0.00 µs | +0.69 µs | pass |
| render_prep_10k_rows_ms | 4.57 ms | 4.46 ms | 0.00 ms | -0.11 ms | pass |
| glyph_raster_us_per_glyph | 0.13 µs | 0.13 µs | 0.00 µs | +0.00 µs | pass |
| scrollback_10k_rows_ms | 13.93 ms | 15.16 ms | 0.00 ms | +1.23 ms | pass |
| unicode_write_ns_per_char | 5.99 ns | 5.94 ns | 0.00 ns | -0.05 ns | pass |
| grid_10m_cells_mb | 196.05 MB | 196.03 MB | 0.00 MB | -0.02 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-12 11:29:06 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 8.96875,
  "ten_panes_ram_mb": 11.296875,
  "input_latency_apply_p95_ms": 4.199416,
  "parse_1m_lines_ms": 754.169625,
  "parse_10m_lines_ms": 10061.271542,
  "grid_10m_cells_mb": 196.03125,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 5.936875,
  "snapshot_frame_us": 6.14208,
  "render_prep_10k_rows_ms": 4.4581669999999995,
  "glyph_raster_us_per_glyph": 0.13239504504504504,
  "scrollback_10k_rows_ms": 15.161667
}
```
