# Catify compatibility suite

Run from repository root:

```bash
python3 compatibility/run.py --cfy ./target/debug/cfy
```

The runner records the pinned upstream Shopify CLI version, executes black-box scenarios, normalizes volatile output (ANSI, paths, timestamps, IDs, versions), and writes a JSON report. Deviations must be listed in `deviations.json`; an unlisted mismatch fails the run.

Live/authenticated commands are deliberately excluded from this fixture suite. They belong in store/theme/app integration tests with explicit credentials.

The black-box scenarios are only one evidence source. See the generated
[`inventory/CLI-PARITY.md`](../inventory/CLI-PARITY.md) report for the complete
111-command status and test coverage matrix. Regenerate and validate it with:

```bash
python3 scripts/generate-cli-matrix.py
python3 scripts/generate-cli-matrix.py --check
```
