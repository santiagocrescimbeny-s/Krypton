# PROGRESS — Krypton (Solana Atomic Launcher & Liquidity Engine)

Última actualización: 2026-10-22 · Base: `main` (`68c3d1e`) = Fases 1-3 + correcciones de las auditorías 2026-10-20/21; rama `phase4-jito-engine`

## Hoja de ruta

| Fase | Descripción | Estado |
|------|-------------|--------|
| 1 | Arquitectura base, seguridad de memoria y entorno | ✅ **Completada y verificada** |
| 2 | Motor asíncrono de metadatos (IPFS / Pinata) | ✅ **VERIFICADA** (auditoría 2026-10-20) |
| 3 | Constructor atómico de transacciones (Create + Buy) | ✅ **COMPLETED & VERIFIED** (auditoría 2026-10-20 run 2; guardas #19/#20/#22 confirmadas por el propietario con 164 tests en `main`) |
| 4 | Enrutamiento Jito Block Engine + ALT (+ simulación previa) | 🚧 **IN PROGRESS** — rama `phase4-jito-engine`: código y tests escritos, gates de `cargo` pendientes |
| 5 | Monitor Yellowstone gRPC + gestor de salida | ⬜ Pendiente |
| 6 | Harness de pruebas, `tracing` y dry-run | ⬜ Pendiente |

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

## Fase 4 — Jito Block Engine y ALT (IN PROGRESS)

Rama `phase4-jito-engine`. Código nuevo: `src/builder/alt.rs`, `src/network/jito.rs`, `JitoError` y
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
- [ ] Cobertura (`cargo llvm-cov --summary-only`), logging redactado (`tracing-subscriber`, #17, Fase 6)

> Nota: la auditoría 2026-10-21 condiciona la Fase 4 a: fusión y gates en `main`, topes #19/#20, `required_balance` (#22)
> y lectura viva de `Global`/`fee_config` (#21), CI en verde con actions fijadas por SHA, rustc 1.97.1 y cobertura medida.
