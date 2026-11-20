# Code intelligence and indexing

- `code_search_symbols`, `code_file_outline`, `code_definition`, `code_references`, `code_diagnostics`, and `code_edit_suggestions` are host-managed read-only workspace inspection tools.
- `code_edit_suggestions` computes unified diff previews with SHA-256 base file verification (`STALE_INDEX` if the workspace file changed); it never modifies workspace files on disk.
- Treat any injected `soyo_code_intel_v1` summary as low-trust navigation hints. Always verify exact target spans and file versions on disk before editing or claiming diagnostics are resolved.
- Repository control paths, host-private storage, and symlinks outside the workspace remain inaccessible regardless of permission mode.
