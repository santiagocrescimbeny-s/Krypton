# PROGRESS — Krypton (Solana Atomic Launcher & Liquidity Engine)

Última actualización: 2026-10-24 · Rama `audit-hardening-fixes` (parches de auditoría #29, #30, #31 en revisión; `main` `9002c6e` fusionado en ella: 519 tests en Windows, 520 esperados en Linux) · Base: `main` = Fases 1-7 + núcleo de la Fase 8 (fusionado desde `phase8-yellowstone-adapter`, `6523991`; 518 tests en Windows, 519 esperados en Linux) · Fase 8 en curso: falta el enlace `tonic`

## Hoja de ruta

| Fase | Descripción | Estado |
|------|-------------|--------|
| 1 | Arquitectura base, seguridad de memoria y entorno | ✅ **Completada y verificada** |
| 2 | Motor asíncrono de metadatos (IPFS / Pinata) | ✅ **VERIFICADA** (auditoría 2026-10-20) |
| 3 | Constructor atómico de transacciones (Create + Buy) | ✅ **COMPLETED & VERIFIED** (auditoría 2026-10-20 run 2; guardas #19/#20/#22 confirmadas por el propietario con 164 tests en `main`) |
| 4 | Enrutamiento Jito Block Engine + ALT (+ simulación previa) | ✅ **COMPLETED & VERIFIED** (213 tests en `main` según el propietario; fusionada en `26d4ecf`) |
| 5 | Monitor Yellowstone gRPC + gestor de salida | ✅ **COMPLETED & VERIFIED** — fusionada en `main` (`94eae34`); hallazgos #28-#34 de la auditoría 2026-10-22 resueltos salvo #32 (adaptador `tonic`, aplazado); CI verde según el propietario: 342 tests, `cargo clippy -D warnings` y `cargo audit` |
| 6 | Harness de simulación, dry-run y observabilidad (`tracing`, métricas) | ✅ **COMPLETED & VERIFIED** — fusionada en `main` (`0291190`); CI verde según el propietario: 390 tests |
| 7 | Trading en vivo e integración con mainnet (barrera dry→live, guardas de capital, tip dinámico) | ✅ **COMPLETED & VERIFIED** — fusionada en `main` desde `phase7-live-trading` (`850463a`); `cargo check`/`clippy -D warnings`/`test --locked` con las crates reales: 452 tests en Windows (el test de permisos `#[cfg(unix)]` se verifica en el CI de Linux) |
| 8 | Adaptador Yellowstone gRPC y feed de tips de Jito | 🟡 **IN PROGRESS** — núcleo del adaptador y feed de tips fusionados en `main` desde `phase8-yellowstone-adapter` (`6523991`) y verificados con `cargo` real (`check`/`clippy -D warnings`/`test --locked`: 518 tests en Windows); **falta el enlace `tonic`** (requiere cambiar `Cargo.lock`), la prueba de humo contra un endpoint real y el `main` (ver «Fase 8») |

## Fase 1 — completada y verificada

Resultado verificado en la auditoría `claude/audit-2026-10-19.md` (rustc 1.97.0 con
`--ignore-rust-version`, necesario antes de fijar el toolchain):

| Gate | Resultado |
|------|-----------|
| `cargo check --all-targets --locked` | OK |
| `cargo clippy --all-targets --locked -- -D warnings` | OK, 0 warnings |
| `cargo test --locked` | OK — **36 tests** (6 unitarios de `parsing` + 30 de integración en `phase1_tests.rs`), 0 fallos |
| `unwrap` / `expect` / `panic!` / `todo!` / `unsafe` en `src/` | 0 (lints `deny` fuera de tests + `#![forbid(unsafe_code)]`) |

Seguridad de memoria implementada:

- `SecretString`, `SecretKeyBytes` y `SecureWallet` implementan `ZeroizeOnDrop`.
- Un test verifica con `read_volatile` que los 64 bytes de la clave quedan en cero tras el `drop`.
- `Debug` de secretos redactado; las URLs se muestran sin ruta ni query.
- Los errores nunca incluyen claves, líneas del `.env` ni URLs.
- `DRY_RUN=true` por defecto.

Pendiente de la Fase 1 (no bloquea esta entrada): medir cobertura (`cargo llvm-cov`); el
requisito "100 % por módulo" sigue sin verificar.

## Housekeeping (hallazgo #14)

- [x] `rust-toolchain.toml` → `channel = "1.97.1"`
- [x] `Cargo.toml` → `rust-version = "1.97.1"`
- [ ] Verificar `cargo check --locked` sin `--ignore-rust-version` (requiere rustc 1.97.1 y acceso a
      crates.io; el entorno cloud de esta sesión no tenía ninguno de los dos)

## Fase 2 — Motor de metadatos (VERIFICADA)

Verificada por la auditoría `claude/audit-2026-10-20.md` sobre el commit `015286e`
(rustc 1.97.0, `RUSTUP_TOOLCHAIN=stable`, `--ignore-rust-version`):

| Gate | Resultado |
|------|-----------|
| `cargo check --all-targets --locked` | OK |
| `cargo clippy --all-targets --locked -- -D warnings` | OK, **0 warnings** |
| `cargo test --locked` | OK — **69 tests** (6 unitarios + 30 `phase1_tests` + 33 `phase2_tests`), 0 fallos |

Código: `src/storage/metadata.rs`, variantes `IpfsError` en `src/errors.rs`, `tests/phase2_tests.rs`,
`tests/common/mod.rs` (servidor HTTP simulado, solo `std`).

- API Pinata v3: `POST {upload}/v3/files`, `multipart/form-data`, `Authorization: Bearer <JWT>`.
- Documento JSON: estándar Metaplex + extras Pump.fun (`showName`, `createdOn`, redes sociales).
- Validación previa a la red, reintentos con backoff, cuerpo de respuesta acotado, mensajes saneados.

Pendiente de la verificación (no bloquea el sello): ejecutar sin `--ignore-rust-version` con rustc 1.97.1
y medir cobertura con `cargo llvm-cov`.

## Fase 3 — Constructor atómico (VERIFICADA)

Código: `src/builder/{constants,pda,math,instructions,transaction}.rs`, `BuilderError` en `src/errors.rs`,
`tests/phase3_tests.rs`. Verificada por la auditoría `claude/audit-2026-10-20-run2.md` sobre el commit `d4ba6a1`
(rustc 1.97.0, `RUSTUP_TOOLCHAIN=stable`, `--ignore-rust-version`): check, clippy `-D warnings` y fmt OK;
**134 tests** en la rama de la Fase 3 (65 de `phase3_tests.rs`). Cuentas, discriminadores y argumentos
coinciden con el IDL oficial de Pump.fun.

| Módulo | Contenido |
|--------|-----------|
| `constants.rs` | Program IDs, seeds, discriminadores, reservas iniciales de la curva, 8 *buyback fee recipients*, reservas de rent/fees |
| `pda.rs` | `global`, `mint_authority`, `bonding_curve`, ABC (ATA de la curva), `metadata` (Metaplex), `fee_config`, `user_volume_accumulator`, `creator_vault`, cuentas *mayhem*, `LaunchAddresses::derive` |
| `math.rs` | Curva producto-constante en `u128` con operaciones *checked*: `BuyPlan::{from_budget, from_token_amount, from_supply_bps}` |
| `instructions.rs` | Codificación Borsh byte a byte: ComputeBudget, ATA idempotente, `create_v2`, `create` (legacy), `buy_v3` |
| `transaction.rs` | `LaunchBuilder`: compila `v0` (con ALT), mide el tamaño exacto (≤ 1232 B), firma; `MintSigner` con semilla en `Zeroizing`; guardas de gasto |

Decisiones de diseño (contra el IDL oficial de `pump-fun/pump-public-docs`):

- **El mint no es un PDA**: es un keypair nuevo por lanzamiento. El PDA es la *mint authority* (`["mint-authority"]`).
- **`create_v2` (Token-2022) + `buy_v3`** por defecto; `create` legacy disponible como `CreateVariant::LegacyCreate`.
  `buy_v3` no necesita leer `Global` por RPC. La ATA del comprador se crea con `CreateIdempotent`.
- Compra de 3,04 SOL (125 bps de comisión supuestos) → **9,76 %** del supply; `max_sol_cost` con 5 % de slippage = 3,192 SOL.
- **Tamaño**: 1212 B sin ALT con URI de 80 caracteres (límite 1232); con URI de 200 caracteres o con el tip de
  Jito (+~49 B) hace falta una ALT (`pda::lookup_table_addresses()`, 905 B con ALT).
- `CurveParams::default()` y `DEFAULT_TOTAL_FEE_BPS = 125` son **supuestos** (hallazgo #21): en la Fase 4 hay que leer
  `Global` y el tramo 0 de `fee_config` por RPC y abortar si difieren.

### Guardas de gasto previas a la Fase 4 (rama `guards-pre-phase4`)

| Hallazgo | Guarda |
|----------|--------|
| #19 tope del priority fee | `MAX_PRIORITY_FEE_LAMPORTS` (opcional, por defecto 5 000 000 = 0,005 SOL; techo absoluto 50 000 000 = 0,05 SOL). `Config::from_vars` rechaza `PRIORITY_FEE_MICRO_LAMPORTS × COMPUTE_UNIT_LIMIT / 1e6` por encima del tope (`config::priority_fee_lamports`, `u128` exacto, redondeo hacia arriba). `LaunchBuilder::new` lo vuelve a comprobar (`BuilderError::PriorityFeeTooHigh`) y nunca acepta más que el techo absoluto |
| #20 cota de slippage | `MAX_SLIPPAGE_BPS` baja de 10 000 a **1 000 (10 %)**, una única constante compartida por `config` y `builder::math`. `LaunchBuilder::new` también rechaza un `BuyPlan` construido a mano con más slippage |
| #22 saldo requerido | `LaunchParams::required_balance(create_rent, tip)`: `max_sol_cost` + rent de la ATA (Token-2022: 2 074 080; legacy: 2 039 280) + `user_volume_accumulator` (1 844 400) + 2 firmas × 5 000 + priority fee + rent de `Create` + tip + reserva *rent-exempt* del pagador (890 880), todo con `checked_add`. `LaunchBuilder::ensure_funded` y `build_signed_checked` (con `FundingSnapshot`) devuelven `BuilderError::InsufficientBalance` **antes** de ensamblar o tocar claves |

Notas: `create_rent` y `tip` los aporta el llamador (el rent de `Create` debe medirse con la simulación de la Fase 4,
no fijarse a mano). Con el objetivo de 3,04 SOL, `required_balance(0, 0)` = 3 196 849 360 lamports.

Verificación de esta entrega: `rustfmt --check` limpio; los módulos de `builder` compilados con `rustc` y revisados con
`clippy-driver -D warnings` contra *mocks* de las crates de Solana, con las aserciones numéricas de los tests nuevos
ejecutadas. **No ejecutado**: `cargo check/clippy/test` con las crates reales (crates.io bloqueado en el sandbox).
Recuento esperado de tests tras la fusión: 6 unitarios + 6 `env_hygiene` + 37 `phase1` + 38 `phase2` + 77 `phase3` = **164**
(146 de la fusión + 18 de las guardas).

## Fase 4 — Jito Block Engine y ALT (COMPLETED & VERIFIED)

Fusionada en `main` (`26d4ecf`); el propietario confirma los 213 tests en verde. Rama de origen `phase4-jito-engine`. Código nuevo: `src/builder/alt.rs`, `src/network/jito.rs`, `JitoError` y
`BuilderError::InvalidLookupTable` en `src/errors.rs`, `tests/phase4_tests.rs` (48 tests).

| Pieza | Contenido |
|-------|-----------|
| `builder/alt.rs` | Instrucciones `CreateLookupTable` / `ExtendLookupTable` codificadas a mano (el SDK 4.1 no las trae), derivación de la dirección de la tabla, decodificador de la cuenta (cabecera de 56 bytes), `launch_table_setup` y `LaunchLookupTable` (tabla comprobada: activa, "caliente" y con las 14 direcciones estáticas) |
| `builder/transaction.rs` | `LaunchBuilder::compile_with_table` (905 B sin tip), `payer()` |
| `network/jito.rs` | `JitoClient` (JSON-RPC sobre `reqwest` + rustls: `getTipAccounts`, `sendBundle` en base64, `getInflightBundleStatuses`), `build_jito_tip_instruction`, `JitoTip`, `assemble_launch_transaction`, `serialize_transaction`, `encode_base64`, `BundleId`, las 8 cuentas de tip oficiales |
| Config | `MAX_JITO_TIP_LAMPORTS` (por defecto 1 000 000; techo absoluto 10 000 000): `JITO_TIP_LAMPORTS` por encima del tope impide arrancar |

Decisiones y garantías:

- **JSON-RPC, no gRPC.** El gRPC del Block Engine exigiría `tonic`/`prost` y protobufs generados, y no se pueden añadir
  crates sin acceso a crates.io (el `Cargo.lock` se edita a mano). `base64` se implementa localmente (vectores RFC 4648 en los tests).
- **El tip nunca va en la ALT** (regla explícita de Jito): la cuenta de tip y el System program quedan como claves estáticas.
  Tamaño esperado con tip: 905 B + ~80 B ≈ 985 B < 1232 B; los tests exigen margen ≥ 200 B.
- **Tope del tip y presupuesto.** `JitoTip` solo se construye entre 1000 lamports y `min(MAX_JITO_TIP_LAMPORTS, techo)`;
  `assemble_launch_transaction` suma el tip al `required_balance` y ejecuta `ensure_funded` antes de compilar o firmar.
  El priority fee conserva su tope (#19) en `LaunchBuilder::new`.
- **Dry-run en la capa más baja:** con `DRY_RUN=true`, `JitoClient::send_bundle` devuelve `JitoError::DryRun` sin tocar la red.
- `sendBundle` **no se reintenta** (ante un timeout hay que consultar el estado); las lecturas sí, con backoff y pausa mínima de 1 s tras un 429.
- Endpoints solo HTTPS (HTTP solo a loopback), sin credenciales/query en la URL, redirecciones desactivadas, respuesta acotada a 64 KiB,
  texto remoto saneado.

Pendiente de la Fase 4 (fuera de esta entrega): simulación previa vía RPC `simulateTransaction` para medir el rent de `Create`,
lectura viva de `Global`/`fee_config` (#21), envío real tras `DRY_RUN=false`, seguimiento del bundle hasta `Landed`.

Verificación de esta entrega: `rustfmt --check` limpio; `alt.rs` y el builder compilados con `rustc` y pasados por
`clippy-driver -D warnings` contra *mocks* de Solana con comprobaciones de comportamiento; las funciones puras de `jito.rs`
(base64, tip, `BundleId`, short_vec) compiladas y ejecutadas aparte. **No ejecutado**: `cargo check/clippy/test` con las crates
reales (crates.io bloqueado en el sandbox), por lo que el cliente HTTP (`reqwest`/`serde`) y `tests/phase4_tests.rs` están sin compilar.
Recuento esperado: 6 unitarios + 6 `env_hygiene` + 38 `phase1` + 38 `phase2` + 77 `phase3` + 48 `phase4` = **213**.

## Fase 5 — Monitor de flujo y gestor de salida (COMPLETED & VERIFIED)

Fusionada en `main` (`94eae34`); el propietario confirmó CI en verde (342 tests, clippy, `cargo audit`). Las notas de verificación de abajo describen el sandbox en el que se escribió, no el estado actual. Código nuevo: `src/monitor/{curve,geyser,exit,submit}.rs`, `src/builder/sell.rs`,
`SellPlan` en `builder/math.rs`, `sell_v3` en `builder/instructions.rs`, variantes `MonitorError` / `ExitError` /
`StreamErrorKind` / `BuilderError::ZeroSellAmount` en `src/errors.rs`, `tests/phase5_tests.rs` (108 tests),
`tests/phase5_submit_tests.rs` (15 tests), `tests/logging_tests.rs` (6 tests), `src/logging.rs` y `docs/yellowstone-adapter.md`.

| Pieza | Contenido |
|-------|-----------|
| `monitor/curve.rs` | Decodificador *zero-copy* de la cuenta `BondingCurve` (discriminador, reservas, `complete`, flags `mayhem`/`cashback`; tolera cuentas antiguas más cortas), `CurveObservation` ordenada por (slot, write_version), `PriceReference`, progreso e índice de precio en bps |
| `monitor/geyser.rs` | Supervisor del stream sobre dos traits (`StreamTransport` / `EventStream`): reconexión con backoff exponencial y *jitter*, watchdog de inactividad, tiempo máximo de conexión, límite de fallos consecutivos, `HealthStatus` por `watch`, apagado limpio; los datos de cuenta/instrucción viajan como `bytes::Bytes` (sin copias) |
| `monitor/exit.rs` | `ExitPlan` (2..=8 tramos entre 1 s y 10 s tras el génesis, stop-loss opcional), `ExitEngine` síncrono y determinista (cotiza cada tramo contra la última observación **más** el impacto de las ventas en vuelo; los tramos fallidos o descartados se acumulan en el siguiente; presupuesto de coste), `ExitManager` (orquesta stream, temporizador y reportes con `tokio::select!`), `run_sell_executor` (envíos concurrentes con `JoinSet`) |
| `monitor/submit.rs` | `JitoSellSubmitter`: `[limit, price, sell_v3, tip]` firmado, `sendBundle` (sin reintento) y sondeo de `getInflightBundleStatuses`; en dry-run el pedido termina como `Simulated` tras firmar |
| `builder/{math,instructions,sell}.rs` | `SellPlan` (`u128` + `checked_*`; comisión redondeada hacia arriba, mínimo de SOL hacia abajo; slippage ≤ `MAX_SLIPPAGE_BPS` = 10 %, hallazgo #20; mínimo cero rechazado), `sell_v3` (mismas 17 cuentas que `buy_v3`; las monedas con *cashback* rechazan v3), `SellBuilder` con tope del priority fee (#19) |

Decisiones y garantías:

- **Sin `tonic`/`prost` en `Cargo.lock`:** el transporte real de Yellowstone no se puede añadir sin acceso a crates.io, así que la
  lógica de supervisión vive tras traits y el adaptador queda especificado en `docs/yellowstone-adapter.md`. Los tests usan un
  transporte guionizado.
- **Sin bloqueos:** todos los canales son acotados; las órdenes se envían con `try_send` a una cola mayor que el máximo de órdenes
  posibles (`ORDER_CHANNEL_CAPACITY`); ningún `.await` mantiene un *guard*; el cierre (`ShutdownTrigger`) también se activa al soltar el emisor.
- **Ventana 1-10 s:** un tramo que llega tarde (más de 500 ms) se descarta (`WindowMissed`) y su cantidad pasa al siguiente.
- **Stop-loss:** barrido de lo que queda con un máximo de 3 intentos y bajo el presupuesto de costes.
- **El apagado no liquida** la posición: informa lo no vendido en el resumen.
- Higiene de memoria: ninguna estructura de la fase guarda claves; el `SecureWallet` se comparte por `Arc` y la firma usa
  `keypair()` solo durante `sign`.
- Cero `unwrap`/`expect`/indexación sin comprobar en `src/` (lints `deny` + `#![forbid(unsafe_code)]`).

Verificación de esta entrega (honesta): crates.io está bloqueado en el sandbox, así que **no** se ejecutó `cargo` real. Se montó un
workspace de prueba con `tokio 1.53.3`, `bytes`, `thiserror 2.0.17` y `tracing` **reales** y *mocks* de las crates de Solana,
`zeroize`, `reqwest` y `serde_json`: `src/monitor/{curve,geyser,exit}.rs`, `builder/{math,sell}.rs` y los **108 tests de
`phase5_tests.rs`** compilan y pasan, y `clippy --all-targets -- -D warnings` sale limpio (con los lints `deny` del crate).
**Sin verificar con las crates reales:** `monitor/submit.rs`, `src/logging.rs`, `tests/phase5_submit_tests.rs` y `tests/logging_tests.rs` (dependen de `reqwest`/`serde`/`tracing-subscriber`),
`SellSettings::from_config`, los tamaños reales de `try_compile`, las firmas reales y la función `from_str_const` de `Pubkey`;
la *feature* `test-util` de tokio como `dev-dependency` (no debería añadir paquetes a `Cargo.lock`, pero hay que confirmarlo con `--locked`).

Recuento esperado tras `cargo test --locked`: 213 (Fases 1-4) + 108 `phase5` + 15 `phase5_submit` + 6 `logging` = **342** (la auditoría esperaba ~320 antes de añadir los tests de #29-#31).

### Correcciones de la auditoría 2026-10-22 (`claude/audit-2026-10-22.md`)

| Hallazgo | Estado | Corrección |
|----------|--------|------------|
| #28 `Cargo.lock` a mano / sin `cargo` real | **ABIERTO** (no se puede cerrar sin crates.io) | `tracing-subscriber` con `env-filter` añade el paquete `matchers`, que no está en `Cargo.lock` y cuyo checksum no puede inventarse: hay que ejecutar **una vez** `cargo update -w` con red y commitear el lock (no editarlo a mano). Hasta entonces `--locked` fallará. Los 3 gates y el recuento real siguen sin ejecutarse |
| #29 blockhash caducado | **Corregido** | El canal publica `TimedBlockhash = (Hash, Instant)` (`blockhash_channel`, `stamp_blockhash`); `submit` rechaza con `ExitError::StaleBlockhash` si `elapsed > MAX_BLOCKHASH_AGE` (40 s) **antes** de firmar o enviar. Tests con reloj pausado (límite inclusivo, recuperación al publicar uno nuevo) |
| #30 sondeo y ventas duplicadas | **Corregido** | Primer `getInflightBundleStatuses` inmediato (el intervalo solo separa consultas). Nuevo `SellOutcome::Unknown` para sondeo agotado, error permanente tras aceptar el bundle y `sendBundle` ambiguo (timeout / reset tras conectar: `JitoError::may_have_been_accepted`). `ExitEngine::report` **no** vende ni reinyecta esos tokens (`EngineSnapshot::tokens_unresolved`); su impacto de precio se mantiene en las cotizaciones `UNRESOLVED_PROJECTION_SLOTS` (160) slots; `resolve_unknown(seq, sold)` los concilia cuando el llamador lee el saldo de la ATA. `Failed` queda solo para fallos definitivos |
| #31 logging | **Corregido** (lock pendiente, ver #28) | `src/logging.rs`: `EnvFilter`, `info` por defecto (`RUST_LOG` > `LOG_LEVEL`), texto plano sin ANSI. Los logs de `monitor` llevan contadores, secuencias y etiquetas estáticas (`ExitError::label`), nunca claves, firmas, montos ni saldos; test que captura **todos** los niveles y lo comprueba |
| #32 adaptador Yellowstone | **APLAZADO** (decisión a confirmar por el propietario) | Requiere `tonic`/`prost`; contrato en `docs/yellowstone-adapter.md`. La Fase 5 no cumple aún el objetivo de ingesta real |
| #33 CI | **Corregido** | Actions fijadas por SHA de commit (`checkout` 4.4.0, `rust-cache` 2.9.3, `install-action` 2.87.30, `gitleaks-action` 2.3.9); `gitleaks-action` no exige licencia en cuentas personales (sí en organizaciones; anotado en el workflow). Falta verlo en verde en GitHub |
| #34 PROGRESS | **Corregido** | Cabecera, SHAs y matriz sincronizados con el estado real |

Pendiente de la Fase 5: adaptador Yellowstone real (`tonic`), cableado en un `main` con `trigger_on_ctrl_c`, lectura viva del
saldo de tokens, y las comprobaciones con `cargo` real descritas arriba.

## Fase 6 — Harness de simulación, dry-run y observabilidad (COMPLETED & VERIFIED)

Rama `phase6-harness-simulation` (desde `main` `94eae34`). Código nuevo: `src/metrics.rs`, `src/simulation/{mod,market,harness}.rs`,
`SimulationError` (y `LauncherError::Simulation`) en `src/errors.rs`, `tests/phase6_tests.rs` (48 tests). Cambios en código existente:
`ExitManager::with_metrics` (`monitor/exit.rs`) y `JitoSellSubmitter::with_metrics` (`monitor/submit.rs`), ambos opcionales y sin efecto si no se usan.

| Pieza | Contenido |
|-------|-----------|
| `simulation/market.rs` | Mercado de *bonding curve* determinista: `SplitMix64` con semilla, flujo de operaciones externas (`generate_flow`), impacto de precio con la matemática de producción (`BuyPlan`/`SellPlan`), reloj de slots monótono y `account_update`, que codifica la cuenta `BondingCurve` de 166 bytes que decodifica el `BondingCurveState` real |
| `simulation/harness.rs` | `SimulationHarness::run`: Genesis (`LaunchBuilder` real: financiación, compilación y firma) → compra atómica → evolución de la curva → salida por tramos (`ExitManager` + `run_sell_executor` reales) sobre el `GeyserMonitor` real. Solo dos fronteras son simuladas: `SimulatedTransport` (stream Geyser) y `SimulatedSubmitter` (Block Engine en memoria). `ScenarioConfig::{calm, volatile, hostile}`, `StressProfile`, `SimulationReport` con `accounting_consistent()` y `fingerprint()` |
| `metrics.rs` | `LatencyHistogram` (40 cubos de potencias de 2, atómico, sin asignaciones, suma saturante), `Stopwatch`, `PipelineMetrics`: instante de recepción de la actualización → orden emitida → transacción ensamblada, en µs con `std::time::Instant`; percentiles p50/p95/p99 |

Frontera de seguridad (nada sale del proceso, ningún tip se paga):

- **Estructural:** `src/simulation` no importa cliente HTTP, cliente del Block Engine ni sockets. Un test (`source_scan_*`) rastrea el código de
  `harness.rs` y `market.rs` buscando `reqwest`, `JitoClient`, `send_bundle`, `TcpStream`, `std::net`, `tonic`, `unsafe`, `unwrap`/`expect`/`panic!`…
- **Configuración:** `SimulationHarness::from_config` rechaza `DRY_RUN=false` con `SimulationError::LiveModeRefused`.
- **Claves:** billetera y *mint* se derivan de la semilla del escenario (públicas por construcción, sin fondos). Las transacciones se
  ensamblan y firman de verdad (para medir la latencia real) y se descartan en memoria. Los tips son **virtuales**: se cuentan en
  `BoundaryReport` y nunca salen del proceso.

Estrés inyectado (todo desde una semilla, reproducible): flujo de fondo y *shocks* explícitos (p. ej. una ballena vendiendo el 90 %),
*jitter* de entrega del stream, caída del stream con snapshot al reconectar, latencia de envío, bundles descartados, resultados `Unknown`
(que a veces sí aterrizaron) y *slippage* real: la orden se cotizó antes y se ejecuta contra la curva actual, y falla si el neto queda por
debajo de `min_sol_output`. Todo con `tokio::time`: con el reloj pausado una ventana de 10 s tarda microsegundos.

Observabilidad: los logs de la simulación llevan solo contadores, secuencias y etiquetas (`simulation finished`, `pipeline latency summary`);
filtro `info` por defecto (`src/logging.rs`). Un test captura **todos** los niveles de una corrida hostil y comprueba que no hay texto con
forma de clave/firma, ni los campos `lamports`/`balance`/`signature`/`secret`, ni ninguna de las cifras de la corrida (posición, proceeds,
reservas). La latencia «ingesta → ensamblado» se mide solo para órdenes disparadas **por una actualización** (stop-loss o tramo que esperaba
su primera observación); las del temporizador se miden de emisión a ensamblado.

Verificación de esta entrega (honesta): crates.io sigue bloqueado en el sandbox, así que **no** se ejecutó `cargo` real ni `--locked`.
En el *workspace* de prueba (tokio, bytes, thiserror y tracing reales; *mocks* de Solana, `zeroize`, `reqwest`, `serde_json`) compilan
`metrics.rs`, `simulation/*`, `monitor/{curve,geyser,exit}.rs`, `builder/*`, y pasan **48 tests de `phase6_tests.rs`** y los 108 de
`phase5_tests.rs`; `clippy --all-targets -- -D warnings` limpio con los lints `deny` del crate. **Sin verificar con las crates reales:**
firmas reales y tamaño real de `try_compile` (el *mock* los falsea; el test del tamaño del launch usa `MAX_TRANSACTION_SIZE` y debe
confirmarse), `Config::from_vars` con `DRY_RUN`, `monitor/submit.rs` (solo cambia una línea de métricas) y la compatibilidad de `[const { AtomicU64::new(0) }; N]` con el MSRV (rustc 1.97.1: debería ser válido).
No se añadieron dependencias ni se tocó `Cargo.lock`.

Recuento esperado tras `cargo test --locked`: 342 (hasta la Fase 5) + 48 `phase6` = **390**.

Pendiente de la Fase 6: correr los tres gates con `cargo` real, regenerar `Cargo.lock` solo si hiciera falta (no debería), y decidir
si el binario `main` expone un subcomando `simulate` (hoy el harness es una librería) y si se añade salida estructurada/archivo.

## Fase 7 — Trading en vivo e integración con mainnet (COMPLETED & VERIFIED)

Rama `phase7-live-trading` (desde `main` `0291190`). Código nuevo: `src/execution/{mod,settings,gate,guards,tip,live}.rs`,
`GuardError`, `LiveError`, `LauncherError::Live` y `ExitError::GuardRejected` en `src/errors.rs`, `tests/phase7_tests.rs` (63 tests).
Cambios en código existente: `Config.live: LiveSettings` y 5 claves de entorno nuevas (`ENV_KEYS` 16 → 21) en `src/config.rs`,
`pub mod execution;` en `src/lib.rs`, aserción de `ENV_KEYS.len()` en `tests/phase1_tests.rs`. No se añadieron dependencias ni se tocó `Cargo.lock`.

| Pieza | Contenido |
|-------|-----------|
| `execution/settings.rs` | `LiveSettings` (`LIVE_TRADING`, `LIVE_CONFIRM`, `LIVE_KEYPAIR_PATH`, `MAX_SOL_PER_TX`, `DAILY_DRAWDOWN_SOL`); todo desactivado por defecto; techos absolutos (10 SOL por tx, 50 SOL al día); `Debug` sin la ruta; los errores no repiten el valor |
| `execution/gate.rs` | Barrera dry→live: `arm()` valida, en orden, `LIVE_TRADING`, `DRY_RUN=false`, la frase de confirmación, ambos límites, la ruta, el archivo de clave (regular, no *symlink*, `0600`, tamaño acotado, UTF-8, formato) y que su clave pública coincida con la billetera configurada; después el techo de *slippage* y la coherencia de límites y tip |
| `execution/guards.rs` | `SpendingGuards`: reserva atómica (`reserve → commit/release`) bajo `Mutex`; tope por transacción, límite diario neto (las reservas abiertas cuentan), *tripwire* que bloquea **entradas** al agotar el día (las **salidas** nunca se bloquean), reinicio diario del contador sin levantar el bloqueo (`rearm()` explícito), retornos que compensan gasto sin crear crédito, techo de *slippage* ≤ 10 % |
| `execution/tip.rs` | `TipPolicy`/`TipBook`: percentil p95 para el lanzamiento y p75 para la salida; amortiguación de picos (techo = 3 × mediana EMA, nunca por debajo del tip base); tope proporcional al nocional (5 %); tope duro `MAX_JITO_TIP_LAMPORTS`; suelo del mínimo de Jito; si no hay observación fresca (≤ 30 s) usa el tip configurado |
| `execution/live.rs` | `BlockEngine` (implementado para `JitoClient`; en tests, un motor simulado), `LiveExecutor`: `launch` (guardas → tip → construcción firmada → envío → sondeo del bundle → liquidación de la reserva), `sell_submitter` (`SellSubmitter` con las mismas guardas) y `run_exit` (monitor Geyser real + `ExitManager` + ejecutor de ventas, genérico sobre `StreamTransport`) |

Liquidación de reservas (la regla que evita doble gasto y fugas): `Landed`/`Unknown` → *commit* (puede haber aterrizado); `Failed` o error
definitivo de envío → *release*; un envío ambiguo (timeout) → `Unknown`, no error. Una venta aterrizada registra además sus ingresos netos como retorno.

Estado de seguridad: sin `unwrap`/`expect`/`panic!`/`unsafe`/`println!` en `src/execution` (test de rastreo de fuentes). Ningún test abre sockets.

Verificación (2026-10-24, crates reales de crates.io, `Cargo.lock` sin cambios) sobre `850463a` en Windows: `cargo fmt --check`,
`cargo check --all-targets --locked` y `cargo clippy --all-targets --locked -- -D warnings` limpios; `cargo test --locked` **452 tests, 0 fallos**
(5 repeticiones completas sin fallos intermitentes). Quedan así verificados con las crates reales los cambios de `src/config.rs` (`Config::from_vars`),
el `impl BlockEngine for JitoClient`, `tests/phase1_tests.rs` (aserción 16 → 21) y el rustfmt de `src/`. `phase7_tests.rs` aporta 62 tests en Windows:
el 63.º, `keypair_file_must_be_private_and_not_a_symlink` (permisos `0600` y *symlinks*), es `#[cfg(unix)]` y solo corre en el CI de Linux.

Límites conocidos (no ocultos):

- **Yellowstone/`tonic` real:** no incluido (hallazgo #32 sigue aplazado, ver `docs/yellowstone-adapter.md`). `run_exit` es genérico sobre `StreamTransport`; el adaptador real se conecta ahí sin tocar el resto.
- **Suelo de tips de Jito:** `TipBook::update` es el punto de inyección; falta el *feed* HTTP/WebSocket y su *parser*. Sin datos frescos se usa `JITO_TIP_LAMPORTS`.
- **Saldo y *blockhash* vivos:** `LaunchRequest` recibe el saldo disponible y el *blockhash*; leerlos del RPC corresponde al `main` que cablee el binario.
- Sin prueba contra mainnet/devnet real: el Block Engine está simulado en la frontera `BlockEngine`.

Recuento tras `cargo test --locked`: 390 + 63 `phase7` = **453** en Linux (452 en Windows, sin el test `#[cfg(unix)]`).

Pendiente tras la Fase 7: adaptador `tonic`, *feed* del suelo de tips y un `main` que cablee todo detrás de la barrera.

## Parches de auditoría #29, #30, #31 (rama `audit-hardening-fixes`, EN REVISIÓN)

Origen: `claude/audit-2026-10-22.md`. Al revisar el código de `main` (`9c00e18`) los tres hallazgos **ya estaban implementados** (Fase 5, correcciones del
22/10) y la auditoría del 23/10 los da por cerrados; esta rama los **re-verifica** y cierra el único resto encontrado.

| Hallazgo | Estado en el código | Tests que lo cubren |
|----------|--------------------|---------------------|
| #29 blockhash caducado | `TimedBlockhash = (Hash, Instant)`; `MAX_BLOCKHASH_AGE = 40 s`; `JitoSellSubmitter::fresh_blockhash` (y el equivalente en `execution/live.rs`) devuelve `ExitError::StaleBlockhash` si `elapsed() > 40 s`, antes de firmar o enviar. `ExitError::StaleBlockhash` existe en `errors.rs` (etiqueta `stale_blockhash`) | `phase5_submit_tests.rs`: `a_stale_blockhash_is_refused_…`, `the_age_limit_is_forty_seconds_inclusive` (40 s pasa, 40 s + 1 ms se rechaza), `publishing_a_new_blockhash_unblocks_the_submitter`; `phase7_tests.rs`: `a_stale_blockhash_stops_a_sell_before_anything_is_reserved` |
| #30 sondeo y resultado desconocido | El primer sondeo es inmediato (`if attempt > 0 { sleep }`); agotar el presupuesto, un envío ambiguo o un error permanente de estado dan `SellOutcome::Unknown`, nunca `Failed`. En `ExitEngine`, `Unknown` queda **retenido** (ni vendido ni arrastrado al siguiente tramo) hasta que el llamador lee el ATA y llama a `resolve_unknown(sequence, sold)` (la lectura viva del saldo aún no está cableada: pendiente junto al `main`) | `phase5_submit_tests.rs`: `the_first_status_check_is_immediate`, `an_unresolved_bundle_is_unknown_not_failed_…`, etc.; `phase5_tests.rs`: `an_unknown_outcome_is_neither_sold_nor_carried_forward`, `an_unknown_order_is_reported_once`, `resolving_an_unknown_order_as_sold_counts_it_once` |
| #31 suscriptor y logs seguros | `logging.rs`: `tracing-subscriber` con `env-filter`, nivel `info` por defecto (`RUST_LOG` lo sustituye), sin rutas de módulo ni ANSI; los eventos solo llevan contadores, secuencias y etiquetas | `logging_tests.rs`; `phase5_tests.rs::logs_never_contain_keys_signatures_amounts_or_balances`; `phase6_tests.rs::simulation_logs_carry_no_keys_signatures_or_amounts` |

Cambio nuevo de esta rama (resto de #31, relacionado con #42 de la auditoría 2026-10-23): `JitoSellSubmitter::submit` registraba el `bundle_id`
(derivado de las firmas) en un `debug!`; ahora solo registra la secuencia. Test de rastreo de fuente: `the_submitter_never_logs_the_bundle_id`.

No tocado a propósito: `tracing-appender` (dependencia sin uso, #42) — quitarla exige regenerar `Cargo.lock` con `cargo` real, y el lockfile no debe editarse a mano.

Verificación: `cargo` real **no** se ejecutó (crates.io bloqueado en el sandbox); el cambio es de una línea de `src/monitor/submit.rs` y un test. Pendiente: los tres gates
(`cargo check --all-targets --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`) en local/CI. Recuento esperado: 453 + 1 = **454** (Linux).

**Verificación con `cargo` real (2026-10-24, Windows):** sobre `2de93f9` (sin la Fase 8) los tres gates en verde con **453 tests**. Tras fusionar `main` (`9002c6e`, núcleo de la Fase 8)
en esta rama, los tres gates siguen en verde con **519 tests**, 0 fallos (518 de `main` + `the_submitter_never_logs_the_bundle_id`); en Linux se esperan 520 por el test `#[cfg(unix)]` de la Fase 7.

## Fase 8 — Adaptador Yellowstone gRPC y feed de tips de Jito (IN PROGRESS)

Rama `phase8-yellowstone-adapter` (desde `main` `9c00e18`). Código nuevo: `src/network/yellowstone.rs`, `src/execution/tip_feed.rs`,
`src/execution/tip_http.rs`; `YellowstoneError`, `TipFeedError` (y sus variantes en `LauncherError`) en `src/errors.rs`; `tests/phase8_tests.rs` (51 tests) y
`tests/phase8_http_tests.rs` (15 tests). **`Cargo.toml` y `Cargo.lock` no se tocan.**

### Decisión sobre `tonic` (bloqueo explícito)

`tonic`, `prost` y `yellowstone-grpc-proto` no están en `Cargo.lock` y crates.io es inalcanzable desde el sandbox; añadirlos exige regenerar el
lockfile con `cargo`, y el lockfile no se edita a mano (ver hallazgo #28). Por eso el adaptador se divide en dos:

| Parte | Estado |
|-------|--------|
| **Núcleo** (`network/yellowstone.rs`): `YellowstoneTransport` (implementa `StreamTransport`), `YellowstoneStream` (implementa `EventStream`), `build_wire_request`, `convert_message`, `classify_grpc_code`, `YellowstoneSettings` | ✅ escrito y probado (sin red) |
| **Enlace** (`YellowstoneLink` / `YellowstoneSession`): `GeyserGrpcClient` de `tonic`, TLS, cabecera `x-token`, copia campo a campo entre los mensajes protobuf y `WireRequest`/`WireMessage` | ⬜ **pendiente**: ~100 líneas, lista de pasos en `docs/yellowstone-adapter.md` («Binding checklist») |

Los tipos `Wire*` son espejos en Rust puro de los campos protobuf que usa el pipeline, de modo que toda decisión (qué instrucciones se conservan, resolución de
direcciones de tablas de búsqueda v0, respuesta a *pings*, clasificación de errores gRPC para decidir reintentos) vive en código que compila y se prueba sin `tonic`.

### Reconexión

La reconexión con *backoff* exponencial y *jitter*, el vigilante de inactividad, el presupuesto de reintentos y el apagado ordenado **ya existen** en
`GeyserMonitor` (Fase 5) y no se duplican: `YellowstoneTransport` es el transporte por el que reconecta. Una credencial rechazada (`UNAUTHENTICATED`,
`PERMISSION_DENIED`) o una suscripción rechazada (`INVALID_ARGUMENT`, `UNIMPLEMENTED`) **no** se reintenta; `UNAVAILABLE`, `RESOURCE_EXHAUSTED`, mensajes malformados y silencio sí.
Los tests lo verifican con el supervisor real y reloj pausado (cada intervalo de espera dentro de su rango con *jitter*: 40-50, 80-100, 160-200, 320-400 ms).

### Feed de tips (hallazgo #38)

`tip_feed.rs` (puro): `TipSource`, `floor_from_sol` (SOL → lamports con redondeo, rechazo de NaN/negativos/>1 SOL/percentiles desordenados), `run_tip_feed`
(primer *fetch* inmediato, intervalo 10 s —mínimo 1 s por el límite del motor público—, plazo de 3 s, *backoff* exponencial hasta 60 s, apagado limpio) que llama a `TipBook::update`.
`tip_http.rs`: `parse_tip_floor` (array de Jito o envoltorio JSON-RPC; importes en SOL) y `HttpTipSource` (`reqwest`, https salvo loopback, sin redirecciones, respuesta ≤ 16 KiB, errores sin texto remoto).
Si el feed falla, el suelo envejece y la política vuelve a `JITO_TIP_LAMPORTS`. Uso: `run_tip_feed(HttpTipSource::mainnet()?, Arc::clone(executor.tips()), TipFeedSettings::default(), shutdown)`.
La URL por defecto (`https://bundles.jito.wtf/api/v1/bundles/tip_floor`) y la forma del documento salen de la especificación pública de la API de Jito; **no se probó contra el servicio real**.

### Verificación de esta entrega (honesta)

`cargo` real **no** se ejecutó. En el *workspace* de prueba (tokio, bytes, thiserror y tracing reales; *mocks* de Solana, `zeroize`, `reqwest` y `serde_json`; *stubs* de `config`/`security`/`network`)
compilan `yellowstone.rs`, `tip_feed.rs` y `errors.rs`, y pasan los **51 tests de `phase8_tests.rs`** más los 6 unitarios, 108 de `phase5`, 48 de `phase6` y 63 de `phase7`; `clippy --all-targets -- -D warnings` limpio.
Incluye el `GeyserMonitor` y el `LiveExecutor::run_exit` reales corriendo sobre `YellowstoneTransport` con conexiones simuladas (rechazo, caída, silencio, basura, credenciales rechazadas).
**Sin compilar ni ejecutar** (dependen de `reqwest`/`serde_json` reales): `src/execution/tip_http.rs` y `tests/phase8_http_tests.rs` (15 tests, contra el servidor HTTP local de `tests/common`).
Tampoco hay prueba alguna contra un endpoint Yellowstone real: la conversión se probó con mensajes construidos a mano según el esquema protobuf publicado, y el enlace `tonic` aún no existe.

Recuento esperado tras `cargo test --locked` sobre esta rama: 453 (`main`, Linux) + 51 + 15 = **519**.

**Verificación con `cargo` real (2026-10-24, Windows, sobre `6523991`):** `cargo check --all-targets --locked`, `cargo clippy --all-targets --locked -- -D warnings`
y `cargo test --locked` en verde: **518 tests**, 0 fallos (452 previos + 51 `phase8` + 15 `phase8_http`; el test `#[cfg(unix)]` de la Fase 7 se verifica en el CI de Linux).
`tip_http.rs` y `phase8_http_tests.rs` quedan así compilados y probados con `reqwest`/`serde_json` reales. Fusionada en `main` por *fast-forward*.

Pendiente de la Fase 8: (1) ~~los tres gates con `cargo` real~~ hecho; (2) enlace `tonic` (añadir `tonic`/`yellowstone-grpc-proto` y regenerar `Cargo.lock` donde crates.io sea alcanzable); (3) prueba de humo contra un endpoint real (cuenta de la curva, *ping*, caída forzada);
(4) un `main` que lea `YELLOWSTONE_ENDPOINT`/`YELLOWSTONE_X_TOKEN`, lance `run_tip_feed` y `run_exit` (la configuración por entorno no se añadió: `ENV_KEYS` queda en 21 y `Config` intacta).

## Correcciones de la auditoría 2026-10-20 (fusionadas en `main`)

Verificadas por la auditoría `claude/audit-2026-10-20-run2.md` sobre `350e404` (check, clippy `-D warnings`,
fmt OK; 81 tests). Hallazgos **cerrados**: #1, #4, #11, #15, #16, #18.

| Hallazgo | Corrección |
|----------|------------|
| #15 copia de la imagen por intento | `ImageAsset` guarda `bytes::Bytes`; `FilePayload` lo comparte y `build_form` usa `Part::stream_with_length(Body::from(bytes.clone()), len)`: clon O(1), sin `to_vec()`. Se declara la longitud para conservar `Content-Length` |
| #16 reintento no idempotente | Documentado en `upload_file` (pines duplicados posibles; mismo CID; mitigaciones) |
| #1 `Config` retiene la clave | `Config` guarda el `SecureWallet` parseado (`wallet()`); el `SecretString` se descarta (y se borra) tras el parseo; `load_wallet()` reconstruye desde los bytes |
| #4 `PRIVATE_KEY` en el entorno | `from_env`/`from_env_file` ejecutan `std::env::remove_var("PRIVATE_KEY")` tras leer, también si el `.env` falla a medias o la validación falla |
| #11 `dotenv()` busca en padres | `dotenvy::from_path` con ruta explícita: `./.env` (`DEFAULT_DOTENV_PATH`) o la ruta pasada a `from_env_file` |
| #18 sin CI | `.github/workflows/ci.yml`: fmt, check, clippy `-D warnings`, test, `cargo audit`, `gitleaks` |

Tests nuevos: 5 en `phase2_tests.rs`, 1 en `phase1_tests.rs`, 6 en `tests/env_hygiene_tests.rs`
(proceso propio: modifican entorno y directorio de trabajo).

`Cargo.lock` se editó a mano para añadir `bytes` (ya estaba en el grafo vía `reqwest`); la auditoría confirmó que
`--locked` lo acepta.

## Hallazgos de auditoría abiertos

Detalle en `claude/audit-2026-10-17.md`, `claude/audit-2026-10-17-run2.md` y
`claude/audit-2026-10-19.md`, `claude/audit-2026-10-20.md`, `claude/audit-2026-10-20-run2.md` y `claude/audit-2026-10-21.md`.

- [x] #1 `Config` retiene la clave como string → guarda solo `SecureWallet` (cerrado, auditoría run 2)
- [x] #2 topes de gasto: priority fee (#19), slippage (#20), saldo requerido (#22) y tope del tip de Jito (`MAX_JITO_TIP_LAMPORTS`, Fase 4; pendiente de re-verificar con `cargo`)
- [x] #3 slippage acotado + matemática u128 → `builder::math` (checked) + cota de 10 %
- [x] #4 limpiar `PRIVATE_KEY` del entorno tras leerla (cerrado)
- [ ] #5–#10 hallazgos Low/Info (`Debug` de wallet, saneado de `Rpc.message`, lints aritméticos, deps sin uso, copias en stack, `redact_url`)
- [x] #11 `dotenvy::dotenv()` recorre directorios padre → ruta explícita (cerrado)
- [ ] #12 campos de `Config` mutables → getters
- [ ] #13 `copy_chunk` devuelve ceros en silencio → `Option`
- [x] #14 toolchain fijado (verificación con rustc 1.97.1 sin `--ignore-rust-version` pendiente)
- [x] #15 copia de la imagen por intento → `bytes::Bytes` (cerrado)
- [x] #16 reintento no idempotente documentado (cerrado)
- [x] #18 CI con `cargo audit` + `gitleaks` (`.github/workflows/ci.yml`; falta verlo en verde en GitHub y fijar las actions por SHA, #26)
- [x] #19 tope del priority fee (pendiente de re-verificar con `cargo`)
- [x] #20 cota de slippage del 10 % (pendiente de re-verificar con `cargo`)
- [ ] #21 parámetros de curva/fees hardcodeados → leer `Global` y `fee_config` por RPC en la Fase 4 y abortar si difieren
- [x] #22 saldo requerido: `required_balance` + `ensure_funded` (el rent de `Create` se mide en la Fase 4)
- [ ] #23 `Config::from_env()` en un `fn main()` síncrono antes del runtime de Tokio (documentado en el README)
- [ ] #24 `MintSigner::from_seed` con `Zeroizing`; #25 `Debug` sin pubkeys; #26 actions por SHA; #27 rustc 1.97.1 y cobertura
- [ ] Cobertura (`cargo llvm-cov --summary-only`); logging (#17/#31): suscriptor con `EnvFilter` añadido en la Fase 5 (pendiente de regenerar `Cargo.lock`), salida estructurada/archivo en la Fase 6

> Nota: la auditoría 2026-10-21 condiciona la Fase 4 a: fusión y gates en `main`, topes #19/#20, `required_balance` (#22)
> y lectura viva de `Global`/`fee_config` (#21), CI en verde con actions fijadas por SHA, rustc 1.97.1 y cobertura medida.
