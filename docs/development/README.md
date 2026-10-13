# Development

How LeandrOS is developed, tested and debugged: the practices, the tools, and the lessons that produced them.

## Pages

- [Workflow](workflow.md) — machine roles chosen by accelerator, lanes as sibling worktrees and waves of integration, the scriptable QEMU driver and its environment variables, and keeping build outputs honest.
- [Testing](testing.md) — the regression suites, how to run them, fresh images, and showing a guard failing before trusting it.
- [Debugging](debugging.md) — `[WDOG]`/`[TIMER]` stall reports, the Ctrl-T task dump, the HVF monitor hazard, serial capture pitfalls, input injection over QMP, and the workflow for changing entry assembly.
- [Methodology](methodology.md) — proving the instrument, measuring at the destination, controls, the accelerator is not the architecture, what QEMU cannot show, and the premises measurement refuted.
- [Conventions](conventions.md) — release builds only, Limine revision 6, both architectures after every change, Rust-only, commit hygiene.
- [Self-hosting](selfhost.md) — building the kernel inside a LeandrOS guest: the staged toolchain, the host↔guest HTTP hand-off, the host/guest comparison, and the kernel bugs it found.

## See also

- [Getting started](../getting-started/README.md) — building and booting.
- [Design principles](../overview/design-principles.md)
- [TODO.md](../../TODO.md) — open work.
