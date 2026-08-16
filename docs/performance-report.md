# Performance Report

- **Commit/version:** 0.1.0
- **Date:** 2026-08-16 17:11:49 UTC

| Metric | Baseline | Current | Budget | Delta | Result |
|--------|----------|---------|--------|-------|--------|
| idle_ram_mb | 9.00 MB | 9.09 MB | 40.00 MB | +0.09 MB | pass |
| ten_panes_ram_mb | 11.41 MB | 11.30 MB | 80.00 MB | -0.11 MB | pass |
| batch_apply_p95_ms | 0.00 ms | 0.00 ms | 0.00 ms | +0.00 ms | pass |
| events_per_second | 18742002.57 ev/s | 18103395.83 ev/s | 0.00 ev/s | -638606.74 ev/s | pass |
| input_to_apply_p95_ms | 0.68 ms | 0.68 ms | 8.00 ms | +0.00 ms | pass |
| shell_echo_p95_ms | 1.39 ms | 1.35 ms | 8.00 ms | -0.04 ms | pass |
| write_to_pty_read_p95_ms | 0.61 ms | 0.61 ms | 0.00 ms | +0.00 ms | pass |
| read_to_apply_p95_ms | 0.13 ms | 0.13 ms | 0.00 ms | +0.00 ms | pass |
| parse_1m_lines_ms | 610.81 ms | 714.39 ms | 0.00 ms | +103.58 ms | pass |
| parse_10m_lines_ms | 11598.36 ms | 12760.51 ms | 0.00 ms | +1162.15 ms | pass |
| snapshot_frame_us | 31.20 µs | 44.73 µs | 0.00 µs | +13.54 µs | pass |
| render_prep_10k_rows_ms | 4.08 ms | 3.87 ms | 0.00 ms | -0.21 ms | pass |
| glyph_raster_us_per_glyph | 0.12 µs | 0.12 µs | 0.00 µs | +0.00 µs | pass |
| scrollback_10k_rows_ms | 37.47 ms | 17.15 ms | 0.00 ms | -20.32 ms | pass |
| unicode_write_ns_per_char | 4.96 ns | 4.67 ns | 0.00 ns | -0.29 ns | pass |
| grid_10m_cells_mb | 196.03 MB | 190.02 MB | 0.00 MB | -6.02 MB | pass |

```json
{
  "commit": "0.1.0",
  "date": "2026-08-16 17:11:49 UTC",
  "startup_ms": 0.0,
  "idle_ram_mb": 9.09375,
  "ten_panes_ram_mb": 11.296875,
  "batch_apply_p95_ms": 0.000042000000000000004,
  "events_per_second": 18103395.82997367,
  "input_to_apply_p95_ms": 0.680708,
  "write_to_pty_read_p95_ms": 0.611708,
  "read_to_apply_p95_ms": 0.132375,
  "shell_echo_p95_ms": 1.3504999999999998,
  "parse_1m_lines_ms": 714.389,
  "parse_10m_lines_ms": 12760.512708,
  "grid_10m_cells_mb": 190.015625,
  "cell_bytes": 16,
  "unicode_write_ns_per_char": 4.670520625,
  "snapshot_frame_us": 44.73292,
  "render_prep_10k_rows_ms": 3.87,
  "glyph_raster_us_per_glyph": 0.11981981981981982,
  "scrollback_10k_rows_ms": 17.146
}
```
