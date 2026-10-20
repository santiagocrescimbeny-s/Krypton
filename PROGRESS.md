# PROGRESS — Krypton (Solana Atomic Launcher & Liquidity Engine)

Última actualización: 2026-10-20 · Base: commit `015286e` (auditoría `audit-2026-10-20`)

## Hoja de ruta

| Fase | Descripción | Estado |
|------|-------------|--------|
| 1 | Arquitectura base, seguridad de memoria y entorno | ✅ **Completada y verificada** |
| 2 | Motor asíncrono de metadatos (IPFS / Pinata) | ✅ **VERIFICADA** (auditoría 2026-10-20) |
| 3 | Constructor atómico de transacciones (Create + Buy) | ⬜ Pendiente |
| 4 | Enrutamiento Jito Block Engine + simulación previa | ⬜ Pendiente |
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

## Correcciones de la auditoría 2026-10-20 (rama `audit-fixes-2026-10-20`)

Sin ejecutar todavía los gates de `cargo` (ver nota al final): los cambios posteriores al commit
auditado añaden tests, por lo que el total de 69 pasará a ser mayor.

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

Para cerrar: `cargo check --all-targets --locked`, `cargo clippy --all-targets --locked -- -D warnings`,
`cargo test --locked` en una máquina con acceso a crates.io. `Cargo.lock` se editó a mano para añadir `bytes`
(ya estaba en el grafo vía `reqwest`).

## Hallazgos de auditoría abiertos

Detalle en `claude/audit-2026-10-17.md`, `claude/audit-2026-10-17-run2.md` y
`claude/audit-2026-10-19.md`.

- [x] #1 `Config` retiene la clave como string → guarda solo `SecureWallet` (pendiente de verificar con cargo)
- [ ] #2 topes de priority fee / tip / gasto total
- [ ] #3 slippage acotado + matemática u128
- [x] #4 limpiar `PRIVATE_KEY` del entorno tras leerla (pendiente de verificar con cargo)
- [ ] #5–#10 hallazgos Low/Info (`Debug` de wallet, saneado de `Rpc.message`, lints aritméticos, deps sin uso, copias en stack, `redact_url`)
- [x] #11 `dotenvy::dotenv()` recorre directorios padre → ruta explícita (pendiente de verificar con cargo)
- [ ] #12 campos de `Config` mutables → getters
- [ ] #13 `copy_chunk` devuelve ceros en silencio → `Option`
- [x] #14 toolchain fijado (verificación local pendiente, ver arriba)
- [x] CI con `cargo audit` + `gitleaks` (`.github/workflows/ci.yml`)
- [ ] Cobertura (`cargo llvm-cov --summary-only`), logging redactado (`tracing-subscriber`, Fase 6)

> Nota: la auditoría condiciona la Fase 2 al cierre de #1–#4 y #11. La Fase 2 se inició por
> decisión explícita del propietario; esos hallazgos siguen abiertos.
