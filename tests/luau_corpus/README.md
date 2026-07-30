# Luau Truth Corpus

This corpus contains 26 standalone Luau sources, from basic literals and
assignments through closures, state machines, register pressure, and a combined
wonky integration case. The final two cases add a product-style controller and
adversarial closure/table dataflow without executing either script.

Run the primary quality matrix:

```powershell
python tools/run_luau_corpus.py --profiles primary
```

Run the compiler-emittable compatibility matrix:

```powershell
python tools/run_luau_corpus.py --profiles compatibility
```

Run one focused case:

```powershell
python tools/run_luau_corpus.py --profiles primary --case 21_state_machine
```

Run the trusted semantic probes across every compiler configuration:

```powershell
python tools/run_luau_corpus.py --profiles all --semantic
```

Run one trusted semantic probe:

```powershell
python tools/run_luau_corpus.py --profiles primary --semantic --case 04_calls_multireturn
```

Every attempt records bytecode, decompiled Luau, compiler diagnostics,
bytecode version, basic output metrics, and a summary. A run succeeds only when
the original compiles, decompilation succeeds, and the decompiled output
recompiles. With `--semantic`, a checked case also requires matching source and
generated runtime results.

Runtime execution is restricted to six committed probes:
`04_calls_multireturn`, `05_varargs`, `13_repeat_until`, `15_generic_for`,
`20_pcall_style_flow`, and `21_state_machine`. Other corpus inputs are never
executed. `27_orchestration_engine` remains a manual, non-blocking diagnostic
input.

Primary profiles are `O1/g1`, `O2/g1`, and `O2/g0`. Compatibility profiles use
the bundled compiler's feature flags to emit bytecode V9, V10, V11, and V12.
Format-level Rust fixtures cover the older supported V4-V8 layouts.
