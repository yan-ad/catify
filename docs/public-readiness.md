# Public-readiness checklist

Crabpify remains **private** until the following are complete and reviewed:

- [x] Rust 1.94 toolchain pin and reproducible local gates.
- [x] Runtime inventory of Shopify CLI 4.6.1 and command ownership report.
- [x] Black-box compatibility suite with explicit deviations.
- [x] Native startup/memory benchmark with methodology and caveats.
- [x] Release archive/checksum tooling and macOS/Linux artifact workflow.
- [x] Security, bug-reporting, contribution, and migration documentation.
- [ ] Production Identity device OAuth with a bundled/approved public client ID.
- [ ] Organization and store live API operations.
- [ ] Remaining authenticated theme remote operations.
- [ ] Remaining authenticated app backend operations.
- [ ] Windows release signing and install smoke.
- [ ] At least one external contributor can build and run the documented smoke commands.
- [ ] Maintainer review of license/dependency policy for every published artifact.

Do not change repository visibility until every unchecked item has an owner, evidence, and a rollback plan.
