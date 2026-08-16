# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-16 19:21:11 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 8.97 MB | 8.98 MB | 40.00 MB | +0.02 MB | pass |
| ten_panes_ram_mb | 11.34 MB | 11.42 MB | 80.00 MB | +0.08 MB | pass |
| batch_apply_p95_ms | 0.00 ms | 0.00 ms | 0.00 ms | +0.00 ms | pass |
| events_per_second | 19131625.68 ev/s | 17988503.73 ev/s | 0.00 ev/s | -1143121.94 ev/s | pass |
| input_to_apply_p95_ms | 0.81 ms | 0.74 ms | 25.00 ms | -0.06 ms | pass |
| shell_echo_p95_ms | 1.60 ms | 1.47 ms | 25.00 ms | -0.13 ms | pass |
| write_to_pty_read_p95_ms | 0.74 ms | 0.63 ms | 0.00 ms | -0.10 ms | pass |
| read_to_apply_p95_ms | 0.16 ms | 0.14 ms | 0.00 ms | -0.01 ms | pass |
| parse_1m_lines_ms | 635.83 ms | 643.64 ms | 0.00 ms | +7.81 ms | pass |
| parse_10m_lines_ms | 11772.04 ms | 11271.65 ms | 0.00 ms | -500.39 ms | pass |
| snapshot_frame_us | 23.53 µs | 32.64 µs | 0.00 µs | +9.11 µs | pass |
| render_prep_10k_rows_ms | 5.82 ms | 14.02 ms | 0.00 ms | +8.20 ms | pass |
| glyph_raster_us_per_glyph | 0.09 µs | 0.09 µs | 0.00 µs | +0.00 µs | pass |
| scrollback_10k_rows_ms | 17.12 ms | 16.84 ms | 0.00 ms | -0.29 ms | pass |
| unicode_write_ns_per_char | 4.97 ns | 4.90 ns | 0.00 ns | -0.07 ns | pass |
| grid_10m_cells_mb | 196.03 MB | 196.03 MB | 0.00 MB | +0.00 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-16 19:21:11 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 8.984375,
  "ten_panes_ram_mb": 11.421875,
  "batch_apply_p95_ms": 0.000042000000000000004,
  "events_per_second": 17988503.733393718,
  "input_to_apply_p95_ms": 0.742584,
  "write_to_pty_read_p95_ms": 0.6325839999999999,
  "read_to_apply_p95_ms": 0.144459,
  "shell_echo_p95_ms": 1.473542,
  "parse_1m_lines_ms": 643.64075,
  "parse_10m_lines_ms": 11271.649958,
  "grid_10m_cells_mb": 196.03125,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 4.901510625,
  "snapshot_frame_us": 32.64,
  "render_prep_10k_rows_ms": 14.018875,
  "glyph_raster_us_per_glyph": 0.09496981981981982,
  "scrollback_10k_rows_ms": 16.835666999999997
}
```
