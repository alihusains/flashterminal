# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-17 05:27:14 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 8.97 MB | 9.00 MB | 40.00 MB | +0.03 MB | pass |
| ten_panes_ram_mb | 11.34 MB | 11.41 MB | 80.00 MB | +0.06 MB | pass |
| batch_apply_p95_ms | 0.00 ms | 0.00 ms | 0.00 ms | +0.00 ms | pass |
| events_per_second | 19131625.68 ev/s | 17859644.68 ev/s | 0.00 ev/s | -1271981.00 ev/s | pass |
| input_to_apply_p95_ms | 0.81 ms | 0.80 ms | 25.00 ms | -0.01 ms | pass |
| shell_echo_p95_ms | 1.60 ms | 1.69 ms | 25.00 ms | +0.09 ms | pass |
| write_to_pty_read_p95_ms | 0.74 ms | 0.74 ms | 0.00 ms | +0.00 ms | pass |
| read_to_apply_p95_ms | 0.16 ms | 0.16 ms | 0.00 ms | +0.00 ms | pass |
| parse_1m_lines_ms | 635.83 ms | 616.38 ms | 0.00 ms | -19.45 ms | pass |
| parse_10m_lines_ms | 11772.04 ms | 11050.16 ms | 0.00 ms | -721.87 ms | pass |
| snapshot_frame_us | 23.53 µs | 34.97 µs | 0.00 µs | +11.44 µs | pass |
| render_prep_10k_rows_ms | 5.82 ms | 3.86 ms | 0.00 ms | -1.96 ms | pass |
| glyph_raster_us_per_glyph | 0.09 µs | 0.10 µs | 0.00 µs | +0.01 µs | pass |
| scrollback_10k_rows_ms | 17.12 ms | 16.62 ms | 0.00 ms | -0.50 ms | pass |
| unicode_write_ns_per_char | 4.97 ns | 4.50 ns | 0.00 ns | -0.47 ns | pass |
| grid_10m_cells_mb | 196.03 MB | 190.02 MB | 0.00 MB | -6.02 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-17 05:27:14 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 9.0,
  "ten_panes_ram_mb": 11.40625,
  "batch_apply_p95_ms": 0.000042000000000000004,
  "events_per_second": 17859644.67517809,
  "input_to_apply_p95_ms": 0.7982090000000001,
  "write_to_pty_read_p95_ms": 0.742083,
  "read_to_apply_p95_ms": 0.155958,
  "shell_echo_p95_ms": 1.6874170000000002,
  "parse_1m_lines_ms": 616.377417,
  "parse_10m_lines_ms": 11050.162541000002,
  "grid_10m_cells_mb": 190.015625,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 4.4958075,
  "snapshot_frame_us": 34.965,
  "render_prep_10k_rows_ms": 3.862,
  "glyph_raster_us_per_glyph": 0.1013891891891892,
  "scrollback_10k_rows_ms": 16.624959
}
```
