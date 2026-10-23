# Krypton — Solana Atomic Launcher & Liquidity Engine

Estado: **Fases 1-4 completadas** (base, metadatos IPFS/Pinata, constructor atómico Create + Buy), consolidadas en `main`; Fase 4 (Jito Block Engine + ALT) completada y verificada; Fase 5 (monitor de flujo + gestor de salida) completada y verificada en `main`; Fase 6 (harness de simulación, dry-run y métricas de latencia) completada y verificada en `main`; Fase 7 (trading en vivo con barrera dry→live, guardas de capital y tip dinámico) completada y verificada en `main`. Ver [`PROGRESS.md`](PROGRESS.md).

## Estructura

```
Cargo.toml                 workspace + versiones centralizadas
rust-toolchain.toml        rustc 1.97.1 fijado + clippy + rustfmt
.env.example               plantilla de variables de entorno
.github/workflows/ci.yml   CI: fmt, check, clippy -D warnings, test, cargo audit, gitleaks
src/
  lib.rs                   lints "zero panics" y declaración de módulos
  errors.rs                LauncherError / ConfigError / WalletError / IpfsError (thiserror)
  parsing.rs               parsers puros (SOL -> lamports, bool, byte array)
  config.rs                Config tipada desde entorno (dotenvy + serde)
  security/
    secret.rs              SecretString (ZeroizeOnDrop, Debug redactado)
    wallet.rs              SecureWallet + SecretKeyBytes (ZeroizeOnDrop)
  storage/metadata.rs      cliente Pinata/IPFS: imagen + JSON Metaplex/Pump.fun -> metadata_uri
  builder/                 Fase 3: constants, pda, math, instructions, transaction (Create + Buy atómico)
    alt.rs                 Fase 4: Address Lookup Table (crear/extender/decodificar, LaunchLookupTable)
  network/jito.rs          Fase 4: cliente JSON-RPC del Block Engine, tip, ensamblado del bundle
  monitor/                 Fase 5: geyser (supervisor del stream), curve, exit (motor de salida por tramos), submit (Jito)
  builder/sell.rs          Fase 5: SellBuilder (sell_v3 firmado)
  docs/yellowstone-adapter.md  Contrato del adaptador gRPC de Yellowstone
  metrics.rs               Fase 6: histogramas de latencia en µs (ingesta del stream → ensamblado de la transacción)
  simulation/              Fase 6: market (curva determinista) y harness (ciclo de vida completo en dry-run, sin red)
  execution/               Fase 7: settings (flags en vivo), gate (barrera dry→live), guards (SpendingGuards), tip (tip dinámico), live (LiveExecutor)
  logging.rs               Fase 5: suscriptor `tracing` con `EnvFilter` (`info` por defecto, sin datos sensibles)
  monitor/                 andamiaje para la fase 5
tests/phase1_tests.rs      pruebas de wallet, zeroize y configuración
tests/phase2_tests.rs      pruebas del motor de metadatos (servidor HTTP simulado)
tests/phase3_tests.rs      pruebas de PDAs, curva, instrucciones, transacción y guardas de gasto
tests/phase4_tests.rs      pruebas de ALT, tip de Jito, tamaño de la tx y cliente (servidor HTTP simulado)
tests/phase6_tests.rs      pruebas del harness de simulación, el mercado simulado y las métricas de latencia (sin red)
tests/phase7_tests.rs      pruebas de la barrera dry→live, las guardas de capital, el tip dinámico y el LiveExecutor (Block Engine simulado, sin red)
tests/env_hygiene_tests.rs higiene del entorno (.env explícito, PRIVATE_KEY limpiada)
tests/common/mod.rs        servidor HTTP de pruebas (solo std)
```

## Puerta de calidad de la fase

```bash
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Configuración

```bash
cp .env.example .env   # y rellenar PRIVATE_KEY, CREATOR_SOL_AMOUNT, etc.
```

El `.env` se lee **solo** desde `./.env` (directorio de trabajo) o desde la ruta dada a
`Config::from_env_file`; nunca se buscan directorios padre. `PRIVATE_KEY` se elimina del
entorno del proceso nada más leerla y `Config` conserva únicamente el `SecureWallet` ya parseado.

Topes de gasto (validados al arrancar y de nuevo en el constructor):

- `SLIPPAGE_BPS` ≤ 1000 (10 %).
- `PRIORITY_FEE_MICRO_LAMPORTS × COMPUTE_UNIT_LIMIT / 1e6` ≤ `MAX_PRIORITY_FEE_LAMPORTS`
  (por defecto 5 000 000 lamports; techo absoluto 50 000 000).
- `JITO_TIP_LAMPORTS` ≤ `MAX_JITO_TIP_LAMPORTS` (por defecto 1 000 000 lamports; techo absoluto 10 000 000).
- Antes de ensamblar la transacción, `LaunchBuilder::ensure_funded` / `build_signed_checked` exigen
  que el pagador tenga el coste máximo (`LaunchParams::required_balance`).

`Config::from_env()` modifica el entorno del proceso (`remove_var`): llámalo desde un `fn main()` síncrono
**antes** de construir el runtime de Tokio.

`DRY_RUN` es `true` por defecto: nada se envía a la red salvo que se desactive
explícitamente.

### Trading en vivo (Fase 7)

`LiveExecutor::arm(&config)` solo arranca si **todo** esto se cumple; si no, devuelve un `LiveError` explícito (nunca entra en pánico):

| Variable | Requisito |
|----------|-----------|
| `DRY_RUN` | `false` (con `LIVE_TRADING=true` y `DRY_RUN=true` → `LiveError::DryRunEnabled`) |
| `LIVE_TRADING` | `true` |
| `LIVE_CONFIRM` | exactamente `i-understand-real-funds-will-be-spent` |
| `LIVE_KEYPAIR_PATH` | archivo regular (no *symlink*), modo `600` en Unix, ≤ 4 KiB; su clave pública debe coincidir con `PRIVATE_KEY` |
| `MAX_SOL_PER_TX` | > 0 y ≤ 10 SOL (tope por transacción) |
| `DAILY_DRAWDOWN_SOL` | ≥ `MAX_SOL_PER_TX` y ≤ 50 SOL (límite diario neto) |

Además, `SLIPPAGE_BPS` ≤ 1000 (10 %) se vuelve a comprobar en la barrera, y el tip dinámico nunca supera `MAX_JITO_TIP_LAMPORTS`.
`ENV_KEYS` pasa de 16 a 21 claves. Los mensajes de error no incluyen rutas ni valores.
