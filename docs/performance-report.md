# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-16 17:43:30 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 8.97 MB | 8.95 MB | 40.00 MB | -0.02 MB | pass |
| ten_panes_ram_mb | 11.34 MB | 11.53 MB | 80.00 MB | +0.19 MB | pass |
| batch_apply_p95_ms | 0.00 ms | 0.00 ms | 0.00 ms | +0.00 ms | pass |
| events_per_second | 19131625.68 ev/s | 18423520.38 ev/s | 0.00 ev/s | -708105.30 ev/s | pass |
| input_to_apply_p95_ms | 0.81 ms | 0.96 ms | 25.00 ms | +0.15 ms | pass |
| shell_echo_p95_ms | 1.60 ms | 1.69 ms | 25.00 ms | +0.09 ms | pass |
| write_to_pty_read_p95_ms | 0.74 ms | 0.76 ms | 0.00 ms | +0.02 ms | pass |
| read_to_apply_p95_ms | 0.16 ms | 0.16 ms | 0.00 ms | +0.00 ms | pass |
| parse_1m_lines_ms | 635.83 ms | 612.66 ms | 0.00 ms | -23.17 ms | pass |
| parse_10m_lines_ms | 11772.04 ms | 11878.39 ms | 0.00 ms | +106.35 ms | pass |
| snapshot_frame_us | 23.53 µs | 31.59 µs | 0.00 µs | +8.06 µs | pass |
| render_prep_10k_rows_ms | 5.82 ms | 3.86 ms | 0.00 ms | -1.96 ms | pass |
| glyph_raster_us_per_glyph | 0.09 µs | 0.10 µs | 0.00 µs | +0.01 µs | pass |
| scrollback_10k_rows_ms | 17.12 ms | 16.31 ms | 0.00 ms | -0.81 ms | pass |
| unicode_write_ns_per_char | 4.97 ns | 4.55 ns | 0.00 ns | -0.42 ns | pass |
| grid_10m_cells_mb | 196.03 MB | 190.00 MB | 0.00 MB | -6.03 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-16 17:43:30 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 8.953125,
  "ten_panes_ram_mb": 11.53125,
  "batch_apply_p95_ms": 0.000042000000000000004,
  "events_per_second": 18423520.37632973,
  "input_to_apply_p95_ms": 0.9585830000000001,
  "write_to_pty_read_p95_ms": 0.76125,
  "read_to_apply_p95_ms": 0.159416,
  "shell_echo_p95_ms": 1.693333,
  "parse_1m_lines_ms": 612.6643750000001,
  "parse_10m_lines_ms": 11878.389208,
  "grid_10m_cells_mb": 190.0,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 4.552056875,
  "snapshot_frame_us": 31.58708,
  "render_prep_10k_rows_ms": 3.856,
  "glyph_raster_us_per_glyph": 0.09981261261261261,
  "scrollback_10k_rows_ms": 16.307540999999997
}
```
