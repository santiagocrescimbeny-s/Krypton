# PROGRESS — Krypton (Solana Atomic Launcher & Liquidity Engine)

Última actualización: 2026-10-20 · Base: commit `83533af` (Fase 1)

## Hoja de ruta

| Fase | Descripción | Estado |
|------|-------------|--------|
| 1 | Arquitectura base, seguridad de memoria y entorno | ✅ **Completada y verificada** |
| 2 | Motor asíncrono de metadatos (IPFS / Pinata) | 🟡 **Implementada — verificación local pendiente** |
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

## Fase 2 — Motor de metadatos (implementada, sin verificar)

Código nuevo: `src/storage/metadata.rs`, variantes `IpfsError` en `src/errors.rs`,
`tests/phase2_tests.rs` (33 tests) y `tests/common/mod.rs` (servidor HTTP simulado, solo `std`).

- API Pinata v3: `POST {upload}/v3/files`, `multipart/form-data` (`file`, `network=public`, `name`),
  `Authorization: Bearer <JWT>`; respuesta `{"data":{"cid":...}}`.
- Documento JSON: estándar Metaplex (`name`, `symbol`, `description`, `image`, `external_url`,
  `attributes`, `properties.files/category`) + extras Pump.fun (`showName`, `createdOn`, `twitter`,
  `telegram`, `website`).
- Validación previa a la red: nombre ≤ 32 B, símbolo ≤ 10 B sin espacios, URI ≤ 200 B, enlaces
  `http(s)`, imagen PNG/JPEG/GIF/WebP por *magic bytes* y ≤ 5 MiB.
- Robustez: reintentos con backoff exponencial (408/425/429/5xx, timeouts, errores de conexión),
  sin seguir redirecciones, cuerpo de respuesta acotado a 64 KiB, mensajes remotos saneados, CID
  validado antes de formar URIs.
- JWT: `SecretString`, cabecera sensible, solo HTTPS (HTTP únicamente en loopback para tests).
- No integrado todavía en `Config`: `PinataConfig::from_vars` lee `PINATA_JWT`, `PINATA_UPLOAD_URL`,
  `IPFS_GATEWAY_URL`, `IPFS_TIMEOUT_SECS`, `IPFS_MAX_RETRIES`.

Para cerrar la Fase 2, en una máquina con acceso a crates.io:

```bash
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Hallazgos de auditoría abiertos

Detalle en `claude/audit-2026-10-17.md`, `claude/audit-2026-10-17-run2.md` y
`claude/audit-2026-10-19.md`.

- [ ] #1 `Config` retiene la clave como string → guardar solo `SecureWallet`
- [ ] #2 topes de priority fee / tip / gasto total
- [ ] #3 slippage acotado + matemática u128
- [ ] #4 limpiar `PRIVATE_KEY` del entorno tras leerla
- [ ] #5–#10 hallazgos Low/Info (`Debug` de wallet, saneado de `Rpc.message`, lints aritméticos, deps sin uso, copias en stack, `redact_url`)
- [ ] #11 `dotenvy::dotenv()` recorre directorios padre → ruta explícita
- [ ] #12 campos de `Config` mutables → getters
- [ ] #13 `copy_chunk` devuelve ceros en silencio → `Option`
- [x] #14 toolchain fijado (verificación local pendiente, ver arriba)
- [ ] Cobertura (`cargo llvm-cov`), logging redactado (`tracing-subscriber`), CI con `cargo audit` + `gitleaks`

> Nota: la auditoría condiciona la Fase 2 al cierre de #1–#4 y #11. La Fase 2 se inició por
> decisión explícita del propietario; esos hallazgos siguen abiertos.
